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
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::Path;
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SUCCESS};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetSecurityInfo, NO_MULTIPLE_TRUSTEE,
    SE_FILE_OBJECT, SetEntriesInAclW, SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
    TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, AddMandatoryAce, CONTAINER_INHERIT_ACE,
    DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetLengthSid, INHERIT_ONLY_ACE, INHERITED_ACE,
    InitializeAcl, IsValidSid, LABEL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE, PSID,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_TAG_INFO, FileAttributeTagInfo,
    GetFileInformationByHandleEx,
};

/// The call that writes security to a target, for messages.
const SET_OP: &str = "SetSecurityInfo";

/// An empty `ACL` header's byte count — the baseline every ACL built here is
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

/// [`INHERIT_ONLY_ACE`] as an `AceFlags` byte, for the same reason as
/// [`INHERITED_ACE_FLAG`].
#[allow(
    clippy::cast_possible_truncation,
    reason = "INHERIT_ONLY_ACE is 0x8, and AceFlags (the field it is tested against) is a single byte"
)]
const INHERIT_ONLY_ACE_FLAG: u8 = INHERIT_ONLY_ACE as u8;

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
/// the baseline a write must clear — it does not remove the check.
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

/// The object a security operation reaches: the very one a held handle
/// names, so what was checked is what is stamped; the path is for messages,
/// and `name` is the text of it a stamp is keyed by.
#[derive(Clone, Copy)]
pub(super) struct Target<'a> {
    pub(super) handle: &'a OwnedHandle,
    pub(super) path: &'a Path,
    pub(super) name: &'a str,
}

impl Target<'_> {
    pub(super) fn is_dir(self) -> io::Result<bool> {
        attribute_tag(self.handle).map(|i| i.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0)
    }
}

#[derive(Clone, Copy)]
pub(super) enum AclKind {
    Dacl,
    Label,
}

/// The ACL borrows its storage from the owned descriptor, held only to be
/// freed after it.
pub(super) struct Security {
    _sd: Local<c_void>,
    pub(super) acl: *mut ACL,
}

impl Security {
    pub(super) fn of(target: Target<'_>, kind: AclKind) -> Result<Self, DaclError> {
        Self::held(target.handle, target.path, kind)
    }

    pub(super) fn held(
        handle: &OwnedHandle,
        path: &Path,
        kind: AclKind,
    ) -> Result<Self, DaclError> {
        // SAFETY: the handle is live with READ_CONTROL; outputs are owned on success.
        Self::query(
            path,
            kind,
            "GetSecurityInfo",
            |info, dacl, sacl, sd| unsafe {
                GetSecurityInfo(
                    handle.as_raw_handle(),
                    SE_FILE_OBJECT,
                    info,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    dacl,
                    sacl,
                    sd,
                )
            },
        )
    }

    /// Run `call` with the information class and the one output slot `kind`
    /// reads, the other null.
    fn query(
        path: &Path,
        kind: AclKind,
        op: &str,
        call: impl FnOnce(u32, *mut *mut ACL, *mut *mut ACL, *mut *mut c_void) -> u32,
    ) -> Result<Self, DaclError> {
        let mut acl = std::ptr::null_mut();
        let mut sd = std::ptr::null_mut();
        let null = std::ptr::null_mut();
        let rc = match kind {
            AclKind::Dacl => call(DACL_SECURITY_INFORMATION, &raw mut acl, null, &raw mut sd),
            AclKind::Label => call(LABEL_SECURITY_INFORMATION, null, &raw mut acl, &raw mut sd),
        };
        if rc != ERROR_SUCCESS {
            return Err(win32_err(path, op, rc));
        }
        // SAFETY: the successful query transferred the descriptor allocation.
        Ok(Self {
            _sd: unsafe { Local::from_raw(sd) },
            acl,
        })
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

/// A held object's attributes and reparse tag, in one query.
#[allow(clippy::cast_possible_truncation, reason = "an eight-byte struct")]
pub(super) fn attribute_tag(handle: &OwnedHandle) -> io::Result<FILE_ATTRIBUTE_TAG_INFO> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO {
        FileAttributes: 0,
        ReparseTag: 0,
    };
    // SAFETY: the handle is live and `info` is exactly the struct the class names.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileAttributeTagInfo,
            (&raw mut info).cast(),
            const { size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32 },
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
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

/// The verdict on a [`set_security`] that wrote a DACL.  Refusal to
/// take ownership of the ACL is its own error: the path is someone else's to
/// stamp, which the user can act on, unlike a bare Win32 code.
fn dacl_written(target: Target<'_>, rc: u32) -> Result<(), DaclError> {
    let (path, op) = (target.path, SET_OP);
    match rc {
        ERROR_SUCCESS => Ok(()),
        ERROR_ACCESS_DENIED => Err(DaclError::WriteDacDenied {
            path: path.to_path_buf(),
            reason: format!("{op}: {}", io::Error::from_raw_os_error(rc.cast_signed())),
        }),
        _ => Err(win32_err(path, op, rc)),
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

/// Merge one explicit allow ACE into `path`'s DACL, under the path mutex the
/// caller holds across scan, ledger, and this; `SetEntriesInAclW` keeps the
/// result in canonical order. A directory gets `OI|CI`. Allow only: a deny ACE
/// naming a capability SID is inert in the `AppContainer` pass of the access
/// check, so there is no deny for this module to stamp (see `super::deny`).
pub(super) fn apply_explicit_ace(
    target: Target<'_>,
    sid_str: &str,
    access_mask: u32,
    inheritable: bool,
) -> Result<(), DaclError> {
    let path = target.path;
    let sid = OwnedSid::parse(sid_str)?;
    let inheritance: u32 = if inheritable {
        OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
    } else {
        0
    };
    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: access_mask,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: inheritance,
        Trustee: trustee_for(&sid),
    };

    let security = Security::of(target, AclKind::Dacl)?;
    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `ea` outlives the call; `existing_dacl` came from the query
    // above, and null there means "no prior DACL", which the callee accepts.
    let rc = unsafe { SetEntriesInAclW(1, &raw const ea, security.acl, &raw mut new_dacl) };
    if rc != ERROR_SUCCESS {
        return Err(win32_err(path, "SetEntriesInAclW", rc));
    }

    // SAFETY: SetEntriesInAclW transferred this allocation.
    let new_dacl = unsafe { Local::from_raw(new_dacl) };
    dacl_written(
        target,
        set_security(
            target,
            DACL_SECURITY_INFORMATION,
            new_dacl.as_ptr(),
            std::ptr::null_mut(),
        ),
    )
}

/// Write one DACL or SACL through the held handle; Windows propagates
/// inheritable ACEs to existing children.
fn set_security(target: Target<'_>, info: u32, dacl: *mut ACL, sacl: *mut ACL) -> u32 {
    // SAFETY: `dacl`/`sacl` are null or fully built ACLs owned by the caller
    // for the call, and the handle is live.
    unsafe {
        SetSecurityInfo(
            target.handle.as_raw_handle(),
            SE_FILE_OBJECT,
            info,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            sacl,
        )
    }
}

/// Stamp a single `SYSTEM_MANDATORY_LABEL_ACE` at [`LOW_INTEGRITY_SID`] into
/// the directory `path`'s SACL, container-inherit only, replacing whatever
/// mandatory label it had — this module ever writes at most one, so there is
/// nothing to merge. Built with [`AddMandatoryAce`] rather than the manual
/// `EXPLICIT_ACCESS_W`/`SetEntriesInAclW` path [`apply_explicit_ace`] takes,
/// since that API only merges DACL entries — mandatory labels are a SACL of
/// their own, and `AddMandatoryAce` is the dedicated primitive for building one.
pub(super) fn apply_mandatory_label_ace(target: Target<'_>) -> Result<(), DaclError> {
    let path = target.path;
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

    let rc = set_security(
        target,
        LABEL_SECURITY_INFORMATION,
        std::ptr::null_mut(),
        acl_ptr,
    );
    if rc != ERROR_SUCCESS {
        return Err(win32_err(path, &format!("{SET_OP}(label)"), rc));
    }
    Ok(())
}

/// Whether `path`'s SACL already carries the mandatory label
/// [`apply_mandatory_label_ace`] stamps: a `SYSTEM_MANDATORY_LABEL_ACE_TYPE`
/// entry for [`LOW_INTEGRITY_SID`] with the no-write-up bit set, inheriting to
/// containers and not to objects. An unreadable SACL answers `false` — attempt
/// the real stamp and let it fail loudly.
pub(super) fn low_integrity_label_present(target: Target<'_>) -> bool {
    let Ok(sid) = OwnedSid::parse(LOW_INTEGRITY_SID) else {
        return false;
    };
    let Ok(security) = Security::of(target, AclKind::Label) else {
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
    target: Target<'_>,
    sid_str: &str,
) -> Result<Vec<PriorAce>, DaclError> {
    let canonical = target.path;
    let sid = OwnedSid::parse(sid_str)?;
    let security = Security::of(target, AclKind::Dacl)?;
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

/// Package principals that grant the `AppContainer` half of its access check.
/// A grant restating them buys nothing, and on an unowned system path would
/// fail `WRITE_DAC` for nothing. Ordinary groups such as `Everyone` grant only
/// the user/group half; they cannot replace a package or capability ACE.
///
/// - `S-1-15-2-1` — `ALL APPLICATION PACKAGES`.
/// - `S-1-15-2-2` — `ALL RESTRICTED APPLICATION PACKAGES`.
const WELL_KNOWN_AC_SIDS: &[&str] = &["S-1-15-2-1", "S-1-15-2-2"];

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
/// is deciding whether such a grant is still needed, and inherit-only ACEs
/// do not, as they apply to children, not to `path`. The walk follows
/// Windows' own access check — a matching deny marks bits off, and a later
/// allow can only add bits not already denied. A NULL DACL admits everyone,
/// but this query still answers 0, as for an empty DACL, so the caller stamps
/// the grant rather than relying on that admission.
fn compute_appcontainer_effective_access(target: Target<'_>) -> Result<u32, DaclError> {
    let path = target.path;
    let security = Security::of(target, AclKind::Dacl)?;
    let mut allowed = 0;
    let mut denied = 0;
    for ace in security.aces() {
        let ace = ace.map_err(|e| win32_err_str(path, &format!("GetAce: {e}")))?;
        if ace.0.AceFlags & INHERIT_ONLY_ACE_FLAG != 0 {
            continue;
        }
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
pub(super) fn appcontainer_already_grants(target: Target<'_>, needed_mask: u32) -> bool {
    match compute_appcontainer_effective_access(target) {
        Ok(effective) => (effective & needed_mask) == needed_mask,
        Err(_) => false,
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
