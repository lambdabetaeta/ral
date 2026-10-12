// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// A port of `filesystem_dacl.rs` from Microsoft's mxc at 0e7c3dd — the
// `DaclManager` apply/restore engine — respelled for ral's error types and
// `windows-sys` bindings, then rekeyed onto path-derived capability SIDs, so
// crash recovery only reclaims profiles. Ported units keep upstream's names
// so each diffs against the one it was taken from.

use super::super::local::Local;
use super::DaclError;
use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SUCCESS};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSidToSidW, DENY_ACCESS, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW,
    NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, AddMandatoryAce, CONTAINER_INHERIT_ACE,
    DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetLengthSid, INHERITED_ACE, InitializeAcl,
    IsValidSid, LABEL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE, PSID,
};

/// An empty `ACL` header's byte count — the floor every ACL built here is
/// sized up from, as the `u32` [`InitializeAcl`] wants.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a fixed eight-byte Win32 header; the size is a compile-time constant nowhere near u32::MAX"
)]
const ACL_CB: u32 = std::mem::size_of::<ACL>() as u32;

/// [`INHERITED_ACE`] as the `u8` an `ACE_HEADER`'s `AceFlags` field is:
/// windows-sys types the flag constants `u32`, but the field is one byte.
#[allow(
    clippy::cast_possible_truncation,
    reason = "INHERITED_ACE is 0x10, and AceFlags (the field it is tested against) is a single byte"
)]
pub(super) const INHERITED_ACE_FLAG: u8 = INHERITED_ACE as u8;

/// `OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE` as the `u8` an `ACE_HEADER`'s
/// `AceFlags` field is, for the same reason as [`INHERITED_ACE_FLAG`].
#[allow(
    clippy::cast_possible_truncation,
    reason = "OI|CI is 0x3, and AceFlags (the field it is tested against) is a single byte"
)]
pub(super) const OICI_ACE_FLAGS: u8 = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8;

/// `CONTAINER_INHERIT_ACE` alone, as an `AceFlags` byte: the inheritance of the
/// mandatory label, which must reach subdirectories but never files (see
/// [`LOW_INTEGRITY_SID`]).
#[allow(
    clippy::cast_possible_truncation,
    reason = "CI is 0x2, and AceFlags, the field it is tested against, is a single byte"
)]
const CI_ACE_FLAGS: u8 = CONTAINER_INHERIT_ACE as u8;

/// `Low`, the mandatory label a `LowBox`/`AppContainer` child's token
/// carries. Windows runs the mandatory-integrity check *before* the
/// DACL/capability-SID pass: a directory with no explicit label defaults to
/// `Medium` with an implicit no-write-up policy, so a `Low`-IL child's create
/// is refused there regardless of any capability ACE the DACL grants. A
/// read-write grant on a directory must therefore also lower its label to
/// `Low`, or no capability SID can ever admit a create. Only directories: the
/// same `Low` on a file would cap the integrity of the process launched from
/// it, and an existing file's write needs no label.
const LOW_INTEGRITY_SID: &str = "S-1-16-4096";

/// `SYSTEM_MANDATORY_LABEL_NO_WRITE_UP`, absent from `windows_sys`'s constant
/// set though [`AddMandatoryAce`] is not. The only policy bit this module
/// sets: a process below the object's label may still read or execute it, it
/// may just not write. This is the same bit Windows implies by default on an
/// unlabeled (`Medium`) object, so stamping it explicitly at `Low` only lowers
/// the floor a write must clear — it does not remove the check.
const MANDATORY_LABEL_NO_WRITE_UP: u32 = 0x1;

/// `SYSTEM_MANDATORY_LABEL_ACE_TYPE`, likewise absent from `windows_sys`
/// though the `SYSTEM_MANDATORY_LABEL_ACE` struct it types is present.
const SYSTEM_MANDATORY_LABEL_ACE_TYPE: u8 = 0x11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AceType {
    Allow,
    Deny,
}

/// The explicit rights and inheritance witnessed by the stamp probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PriorAce {
    pub(super) ace_type: AceType,
    pub(super) access_mask: u32,
    /// Raw `AceFlags` with `INHERITED_ACE` masked off.
    pub(super) inherit_flags: u8,
}

/// Owned PSID from `ConvertStringSidToSidW`, freed with `LocalFree`.
pub(super) struct OwnedSid(Local<c_void>);

// SAFETY: the buffer is a private, immutable `LocalAlloc` — nothing else in
// the process holds or mutates it — and every use is a read-only Win32 call
// (`EqualSid`, `AddAccessAllowedAceEx`, …), so `well_known_ac_sids`'s
// process-wide cache may share the pointer across threads. The `LocalFree`
// still belongs to whichever `OwnedSid` is dropped.
#[allow(
    clippy::non_send_fields_in_send_ty,
    reason = "the SID is immutable LocalAlloc storage with no thread affinity"
)]
unsafe impl Send for OwnedSid {}
unsafe impl Sync for OwnedSid {}

/// The SID grammar tops out well under 200 characters (15 sub-authorities of
/// ~10 digits); 256 turns away junk before the Win32 parser sees it.
const MAX_SID_STRING_LEN: usize = 256;

impl OwnedSid {
    pub(super) fn parse(s: &str) -> Result<Self, DaclError> {
        if s.is_empty() {
            return Err(DaclError::InvalidSid("(empty)".to_string()));
        }
        if s.len() > MAX_SID_STRING_LEN {
            return Err(DaclError::InvalidSid(format!(
                "SID string too long ({} bytes, max {MAX_SID_STRING_LEN})",
                s.len(),
            )));
        }
        let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
        let mut psid: PSID = std::ptr::null_mut();
        // SAFETY: `wide` is NUL-terminated and outlives the call; `psid` is an
        // out-param the callee fills on success.
        let ok = unsafe { ConvertStringSidToSidW(wide.as_ptr(), &raw mut psid) };
        if ok == 0 {
            return Err(DaclError::InvalidSid(format!(
                "{s}: {}",
                io::Error::last_os_error()
            )));
        }
        // SAFETY: ConvertStringSidToSidW transferred this allocation.
        let sid = Self(unsafe { Local::from_raw(psid) });
        if psid.is_null() || unsafe { IsValidSid(psid) } == 0 {
            return Err(DaclError::InvalidSid(s.to_string()));
        }
        Ok(sid)
    }

    pub(super) fn as_psid(&self) -> PSID {
        self.0.as_ptr()
    }
}

#[derive(Clone, Copy)]
pub(super) enum AclKind {
    Dacl,
    Label,
}

/// The ACL borrows its storage from the owned descriptor.
pub(super) struct Security {
    sd: Local<c_void>,
    pub(super) acl: *mut ACL,
}

impl Security {
    pub(super) fn named(path: &Path, kind: AclKind) -> Result<Self, DaclError> {
        let label = matches!(kind, AclKind::Label);
        let path_w = wide(path);
        let mut acl = std::ptr::null_mut();
        let mut sd = std::ptr::null_mut();
        // SAFETY: the path is terminated; outputs are owned only on success.
        let rc = unsafe {
            GetNamedSecurityInfoW(
                path_w.as_ptr(),
                SE_FILE_OBJECT,
                if label {
                    LABEL_SECURITY_INFORMATION
                } else {
                    DACL_SECURITY_INFORMATION
                },
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                if label {
                    std::ptr::null_mut()
                } else {
                    &raw mut acl
                },
                if label {
                    &raw mut acl
                } else {
                    std::ptr::null_mut()
                },
                &raw mut sd,
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(win32_err(path, "GetNamedSecurityInfoW", rc));
        }
        // SAFETY: the successful query transferred the descriptor allocation.
        Ok(Self {
            sd: unsafe { Local::from_raw(sd) },
            acl,
        })
    }

    pub(super) fn of_file(file: &std::fs::File, path: &Path) -> Result<Self, DaclError> {
        let mut acl = std::ptr::null_mut();
        let mut sd = std::ptr::null_mut();
        // SAFETY: file is live with READ_CONTROL; outputs are owned on success.
        let rc = unsafe {
            windows_sys::Win32::Security::Authorization::GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut acl,
                std::ptr::null_mut(),
                &raw mut sd,
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(win32_err(path, "GetSecurityInfo", rc));
        }
        // SAFETY: the successful query transferred the descriptor allocation.
        Ok(Self {
            sd: unsafe { Local::from_raw(sd) },
            acl,
        })
    }

    pub(super) fn protected(&self) -> io::Result<bool> {
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: sd owns a valid descriptor for the duration of this call.
        if unsafe {
            windows_sys::Win32::Security::GetSecurityDescriptorControl(
                self.sd.as_ptr(),
                &raw mut control,
                &raw mut revision,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(control & windows_sys::Win32::Security::SE_DACL_PROTECTED != 0)
    }

    pub(super) fn aces(&self) -> impl Iterator<Item = io::Result<Ace<'_>>> {
        // SAFETY: a non-null ACL is part of the owned, queried descriptor.
        let count = if self.acl.is_null() {
            0
        } else {
            unsafe { (*self.acl).AceCount }
        };
        (0..count).map(move |index| {
            let mut ptr = std::ptr::null_mut();
            // SAFETY: the index is in range and the descriptor outlives the iterator.
            if unsafe { GetAce(self.acl, u32::from(index), &raw mut ptr) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: every ACE starts with a header, borrowed from self.
            Ok(Ace(unsafe { &*ptr.cast::<ACE_HEADER>() }))
        })
    }
}

pub(super) struct Ace<'a>(pub(super) &'a ACE_HEADER);

impl Ace<'_> {
    pub(super) fn kind(&self) -> Option<AceType> {
        match self.0.AceType {
            0 => Some(AceType::Allow),
            1 => Some(AceType::Deny),
            _ => None,
        }
    }

    #[allow(
        clippy::cast_ptr_alignment,
        reason = "Win32 ACLs DWORD-align each ACE, including its header"
    )]
    fn mask_and_sid(&self) -> Option<(u32, PSID)> {
        if !matches!(self.0.AceType, 0 | 1 | SYSTEM_MANDATORY_LABEL_ACE_TYPE) {
            return None;
        }
        let ace = std::ptr::from_ref(self.0).cast::<ACCESS_ALLOWED_ACE>();
        // SAFETY: allow, deny and mandatory-label ACEs share this prefix.
        Some(unsafe { ((*ace).Mask, (&raw const (*ace).SidStart).cast_mut().cast()) })
    }

    pub(super) fn mask_for(&self, sid: &OwnedSid) -> Option<u32> {
        let (mask, ace_sid) = self.mask_and_sid()?;
        // SAFETY: both SIDs belong to live owners.
        (unsafe { EqualSid(ace_sid, sid.as_psid()) } != 0).then_some(mask)
    }
}

pub(super) fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn trustee_for(sid: &OwnedSid) -> TRUSTEE_W {
    TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        // The trustee-by-SID form reinterprets this field as the PSID; both
        // are raw `*mut _` here, so the cast preserves the bits.
        ptstrName: sid.as_psid().cast::<u16>(),
    }
}

/// The verdict on a `SetNamedSecurityInfoW` that wrote a DACL.  Refusal to
/// take ownership of the ACL is its own error: the path is someone else's to
/// stamp, which the user can act on, unlike a bare Win32 code.
fn dacl_written(path: &Path, rc: u32) -> Result<(), DaclError> {
    match rc {
        ERROR_SUCCESS => Ok(()),
        ERROR_ACCESS_DENIED => Err(DaclError::WriteDacDenied {
            path: path.to_path_buf(),
            reason: format!(
                "SetNamedSecurityInfoW: {}",
                io::Error::from_raw_os_error(rc.cast_signed())
            ),
        }),
        _ => Err(win32_err(path, "SetNamedSecurityInfoW", rc)),
    }
}

pub(super) fn win32_err(path: &Path, op: &str, rc: u32) -> DaclError {
    DaclError::Win32 {
        path: path.to_path_buf(),
        reason: format!("{op}: {}", io::Error::from_raw_os_error(rc.cast_signed())),
    }
}

pub(super) fn win32_err_str(path: &Path, msg: &str) -> DaclError {
    DaclError::Win32 {
        path: path.to_path_buf(),
        reason: msg.to_string(),
    }
}

pub(super) fn null_deny_dacl(path: &Path) -> DaclError {
    win32_err_str(
        path,
        "cannot add a deny to a null DACL without removing ordinary access; give this path explicit permissions first",
    )
}

/// Merge one explicit allow-or-deny ACE into `path`'s DACL, under the path
/// mutex the caller holds across scan, ledger, and this. `SetEntriesInAclW`
/// inserts in canonical order — explicit deny, explicit allow, inherited — so
/// a deny nested inside an allowed parent beats the allow the parent
/// inherits down to it. The other direction never reaches this backend at
/// all: `SandboxProjection::of` calls
/// [`Table::live`](crate::capability::table::Table::live) so an allow
/// beneath a deny is dropped from the projection before any ACE is stamped,
/// rather than relying on ACL order to bury it. A directory gets `OI|CI`;
/// [`super::deny::descendants`] reasserts denies at descendants where inheritance
/// cannot suffice, without changing their protection or ordinary permissions.
pub(super) fn apply_explicit_ace(
    path: &Path,
    sid_str: &str,
    access_mask: u32,
    inheritable: bool,
    ace_type: AceType,
) -> Result<(), DaclError> {
    let sid = OwnedSid::parse(sid_str)?;
    let inheritance: u32 = if inheritable {
        OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
    } else {
        0
    };
    let access_mode = match ace_type {
        AceType::Allow => GRANT_ACCESS,
        AceType::Deny => DENY_ACCESS,
    };
    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: access_mask,
        grfAccessMode: access_mode,
        grfInheritance: inheritance,
        Trustee: trustee_for(&sid),
    };

    let path_w = wide(path);
    let security = Security::named(path, AclKind::Dacl)?;
    if security.acl.is_null() && ace_type == AceType::Deny {
        return Err(null_deny_dacl(path));
    }

    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `ea` outlives the call; `existing_dacl` came from the query
    // above, and null there means "no prior DACL", which the callee accepts.
    let rc = unsafe { SetEntriesInAclW(1, &raw const ea, security.acl, &raw mut new_dacl) };
    if rc != ERROR_SUCCESS {
        return Err(win32_err(path, "SetEntriesInAclW", rc));
    }

    // SAFETY: SetEntriesInAclW transferred this allocation.
    let new_dacl = unsafe { Local::from_raw(new_dacl) };
    // SAFETY: `new_dacl` was just built by SetEntriesInAclW.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl.as_ptr(),
            std::ptr::null_mut(),
        )
    };
    dacl_written(path, rc)
}

/// Stamp a single `SYSTEM_MANDATORY_LABEL_ACE` at [`LOW_INTEGRITY_SID`] into
/// the directory `path`'s SACL, container-inherit only, replacing whatever
/// mandatory label it had — this module ever writes at most one, so there is
/// nothing to merge. Built with [`AddMandatoryAce`] rather than the manual
/// `EXPLICIT_ACCESS_W`/`SetEntriesInAclW` path [`apply_explicit_ace`] takes,
/// since that API only merges DACL entries — mandatory labels are a SACL of
/// their own, and `AddMandatoryAce` is the dedicated primitive for building one.
pub(super) fn apply_mandatory_label_ace(path: &Path) -> Result<(), DaclError> {
    let sid = OwnedSid::parse(LOW_INTEGRITY_SID)?;

    // SAFETY: `sid` owns a valid PSID for the duration of this call.
    let sid_len: u32 = unsafe { GetLengthSid(sid.as_psid()) };
    // Per-ACE byte size: ACE_HEADER (4) + ACCESS_MASK (4) + SID bytes.
    let ace_size: u32 = 8 + sid_len;
    let mut acl_size: u32 = ACL_CB + ace_size;
    acl_size = (acl_size + 3) & !3; // DWORD-align, per ACL_SIZE_INFORMATION's units

    let dwords = (acl_size as usize).div_ceil(4);
    let mut acl_buf: Vec<u32> = vec![0u32; dwords];
    let acl_ptr = acl_buf.as_mut_ptr().cast::<ACL>();

    // SAFETY: freshly allocated, zeroed, 4-byte-aligned, and at least
    // `acl_size` bytes long.
    if unsafe { InitializeAcl(acl_ptr, acl_size, ACL_REVISION) } == 0 {
        return Err(win32_err_str(
            path,
            &format!("InitializeAcl: {}", io::Error::last_os_error()),
        ));
    }
    // SAFETY: `acl_ptr` was sized above to hold exactly one mandatory-label
    // ACE; `sid` is a valid PSID.
    if unsafe {
        AddMandatoryAce(
            acl_ptr,
            ACL_REVISION,
            CONTAINER_INHERIT_ACE,
            MANDATORY_LABEL_NO_WRITE_UP,
            sid.as_psid(),
        )
    } == 0
    {
        return Err(win32_err_str(
            path,
            &format!("AddMandatoryAce: {}", io::Error::last_os_error()),
        ));
    }

    let path_w = wide(path);
    // SAFETY: `acl_ptr` is a fully built SACL owned by this stack frame;
    // `path_w` is NUL-terminated and outlives the call.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl_ptr,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(win32_err(path, "SetNamedSecurityInfoW(label)", rc));
    }
    Ok(())
}

/// Whether `path`'s SACL already carries the mandatory label
/// [`apply_mandatory_label_ace`] stamps: a `SYSTEM_MANDATORY_LABEL_ACE_TYPE`
/// entry for [`LOW_INTEGRITY_SID`] with the no-write-up bit set, inheriting to
/// containers and not to objects. An unreadable SACL answers `false` — attempt
/// the real stamp and let it fail loudly.
pub(super) fn low_integrity_label_present(path: &Path) -> bool {
    let Ok(sid) = OwnedSid::parse(LOW_INTEGRITY_SID) else {
        return false;
    };
    let Ok(security) = Security::named(path, AclKind::Label) else {
        return false;
    };
    security.aces().any(|ace| {
        ace.is_ok_and(|ace| {
            ace.0.AceType == SYSTEM_MANDATORY_LABEL_ACE_TYPE
                && ace.0.AceFlags & OICI_ACE_FLAGS == CI_ACE_FLAGS
                && ace
                    .mask_for(&sid)
                    .is_some_and(|mask| mask & MANDATORY_LABEL_NO_WRITE_UP != 0)
        })
    })
}

/// Every explicit (non-inherited) ACE on `canonical` for `sid_str`. The
/// stamp-probe reads it lock-free — a stale answer there costs one redundant
/// stamp. The descriptor owns every ACE until the scan finishes.
pub(super) fn scan_explicit_aces_for_sid(
    canonical: &Path,
    sid_str: &str,
) -> Result<Vec<PriorAce>, DaclError> {
    let sid = OwnedSid::parse(sid_str)?;
    let security = Security::named(canonical, AclKind::Dacl)?;
    security
        .aces()
        .filter_map(|ace| {
            let ace = match ace {
                Ok(ace) => ace,
                Err(e) => return Some(Err(win32_err_str(canonical, &format!("GetAce: {e}")))),
            };
            if ace.0.AceFlags & INHERITED_ACE_FLAG != 0 {
                return None;
            }
            Some(Ok(PriorAce {
                ace_type: ace.kind()?,
                access_mask: ace.mask_for(&sid)?,
                inherit_flags: ace.0.AceFlags,
            }))
        })
        .collect()
}

/// SIDs every `AppContainer` token implicitly carries, so a grant restating
/// one of them buys nothing — and on a system path this account does not own
/// would fail `WRITE_DAC` for nothing.
///
/// - `S-1-15-2-1` — `ALL APPLICATION PACKAGES`.
/// - `S-1-15-2-2` — `ALL RESTRICTED APPLICATION PACKAGES`.
/// - `S-1-1-0` — `Everyone`, which an `AppContainer` token does keep; it
///   strips `Authenticated Users` and `Users`, so those are absent by design.
const WELL_KNOWN_AC_SIDS: &[&str] = &["S-1-15-2-1", "S-1-15-2-2", "S-1-1-0"];

fn well_known_ac_sids() -> &'static [OwnedSid] {
    static CACHE: std::sync::OnceLock<Vec<OwnedSid>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            WELL_KNOWN_AC_SIDS
                .iter()
                .map(|s| OwnedSid::parse(s).expect("well-known AC SID must parse"))
                .collect()
        })
        .as_slice()
}

/// What [`WELL_KNOWN_AC_SIDS`] alone grant on `path`: inherited ACEs count,
/// explicit grants to a specific `AppContainer` SID do not, since the caller
/// is deciding whether such a grant is still needed. The walk follows
/// Windows' own access check — a matching deny marks bits off, and a later
/// allow can only add bits not already denied. A NULL or empty DACL yields 0,
/// "grants nothing", so the caller tries the real grant instead of assuming.
fn compute_appcontainer_effective_access(path: &Path) -> Result<u32, DaclError> {
    let security = Security::named(path, AclKind::Dacl)?;
    let mut allowed = 0;
    let mut denied = 0;
    for ace in security.aces() {
        let ace = ace.map_err(|e| win32_err_str(path, &format!("GetAce: {e}")))?;
        let Some(kind) = ace.kind() else { continue };
        let Some(mask) = well_known_ac_sids()
            .iter()
            .find_map(|sid| ace.mask_for(sid))
        else {
            continue;
        };
        match kind {
            AceType::Deny => denied |= mask & !allowed,
            AceType::Allow => allowed |= mask & !denied,
        }
    }
    Ok(allowed)
}

/// Whether `needed_mask` is already covered without any per-session ACE.
/// A DACL that cannot be read answers `false`: better to attempt the grant
/// and fail than to assume a coverage nothing verified.
///
/// after mxc `fallback_detector.rs::appcontainer_already_grants`
fn appcontainer_already_grants(path: &Path, needed_mask: u32) -> bool {
    match compute_appcontainer_effective_access(path) {
        Ok(effective) => (effective & needed_mask) == needed_mask,
        Err(_) => false,
    }
}

/// Drop the paths already covered by the well-known SIDs. Without this a
/// non-admin session stamping a system root — `System32`, which
/// `ALL APPLICATION PACKAGES` reads system-wide anyway — would fail closed on
/// `WRITE_DAC` for a grant nobody needed. Grants only: `session::confine`
/// never filters deny paths, since a group grant cannot subtract access.
///
/// after mxc `dispatcher.rs::filter_paths_needing_grant`
pub(crate) fn filter_paths_needing_grant(paths: Vec<PathBuf>, needed_mask: u32) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter(|p| !appcontainer_already_grants(p, needed_mask))
        .collect()
}

#[cfg(test)]
mod tests;
