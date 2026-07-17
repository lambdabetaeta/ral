// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// This module imitates `appcontainer_runner.rs`'s
// `AppContainerScriptRunner` (profile lifecycle, capability-SID derivation)
// and `string_util.rs`'s `sid_to_string`, both from
// github.com/microsoft/mxc @ 0e7c3dd, adapted to ral's `windows-sys` binding
// and single-spawn-boundary design. Each ported unit carries a
// `// after mxc <file>::<fn>` breadcrumb naming its upstream counterpart,
// for future diffing against upstream.

//! AppContainer profile lifecycle and LowBox spawn capabilities.
//!
//! A shell session registers one AppContainer profile per distinct fs
//! projection it confines (`session` owns the keying and naming), each
//! created the first time its projection becomes enforceable and deleted at
//! session end. This module owns the profile lifecycle primitives plus the
//! `SECURITY_CAPABILITIES` construction that a confined spawn threads
//! through
//! [`crate::process::launch::Launch::security_capabilities`]. Filesystem
//! grants (ACE stamping on host paths for the profile's SID) are `dacl`'s
//! job, not this module's; net capability inclusion here is the entire
//! enforcement of `net: false` — an AppContainer with neither
//! `internetClient` nor `privateNetworkClientServer` cannot open a socket.
//!
//! Ported function-for-function from MXC's Tier 3 `AppContainerScriptRunner`
//! where it fits our single-spawn-boundary design (breadcrumbs at each
//! mirrored unit); the CreateProcessW / job-object / suspend-resume dance
//! itself belongs to `process::launch`, not here — this module only builds
//! the profile and the capability array those callers pass in.
//!
//! ## Deviations from MXC Tier 3
//!
//! 1. **No LPAC.** Upstream conditionally sets
//!    `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY` to opt the
//!    child out of `ALL APPLICATION PACKAGES` grants when its
//!    `least_privilege_mode` is requested (`appcontainer_runner.rs::
//!    spawn_suspended`, 0e7c3dd). We never set it: our children are
//!    arbitrary host tools, and LPAC denies exactly the paths the
//!    `ALL APPLICATION PACKAGES` group is granted read access to system-wide
//!    (fonts, common DLLs, …) that those tools expect to read. LPAC is for
//!    known, packaged binaries — not applicable here.
//! 2. **No Win32k-syscall-disable mitigation.** Upstream conditionally sets
//!    `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY` to
//!    `PROCESS_CREATION_MITIGATION_POLICY_WIN32K_SYSTEM_CALL_DISABLE_ALWAYS_ON`
//!    when UI isolation is requested (`process_mitigation.rs`, 0e7c3dd). We
//!    never set it for the same reason: it assumes a headless, known binary,
//!    and disabling Win32k syscalls breaks arbitrary host tools that touch
//!    window-station/desktop API surface incidentally (console resize
//!    probes, clipboard, …).
//!
//! Everything else — profile create/reuse/delete, SID derivation, capability
//! SID derivation and ownership — stays close to upstream.

use std::io;
use std::ptr;

use windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS;
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{DeriveCapabilitySidsFromName, FreeSid, PSID, SID_AND_ATTRIBUTES};
use windows_sys::Win32::System::SystemServices::SE_GROUP_ENABLED;

// `windows-sys` generates `PSID`/`HLOCAL` as the same underlying
// `*mut c_void` type alias, so a `PSID` can be passed directly where
// `LocalFree` expects an `HLOCAL` with no cast.
use windows_sys::Win32::Foundation::LocalFree;

/// `HRESULT_FROM_WIN32` from `winerror.h`, reproduced because it is a
/// header-only macro that `windows-sys` does not generate. Only used to
/// recognize [`CreateAppContainerProfile`]'s already-exists HRESULT, so the
/// general "already an HRESULT" case the full macro handles (`code` treated
/// as already negative) is not needed here — every input is a genuine Win32
/// error code.
// `AppContainerProfile::create_or_reuse` is this function's only non-test
// caller; see the `impl AppContainerProfile` block below.
const fn hresult_from_win32(code: u32) -> i32 {
    ((code & 0x0000_FFFF) | (0x7 << 16) | 0x8000_0000) as i32
}

fn hresult_err(context: &str, hr: i32) -> io::Error {
    io::Error::other(format!("{context} failed: hresult=0x{:08X}", hr as u32))
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Read a null-terminated UTF-16 string out of a raw pointer the OS
/// allocated (e.g. `ConvertSidToStringSidW`'s output). The caller frees the
/// backing memory.
///
/// # Safety
/// `ptr` must be a valid pointer to a null-terminated UTF-16 string.
unsafe fn wide_ptr_to_string(ptr: *const u16) -> String {
    let mut len = 0usize;
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    String::from_utf16_lossy(slice)
}

// after mxc wxc_common/string_util.rs::sid_to_string (0e7c3dd)
/// Render a `PSID` as its `S-1-...` string form via `ConvertSidToStringSidW`,
/// freeing the OS-allocated output string. Shared by
/// [`AppContainerProfile::sid_string`] and anything else in this backend
/// that needs a SID's textual form (ACE targeting, session ledger keys).
fn sid_to_string(sid: PSID) -> io::Result<String> {
    let mut out: *mut u16 = ptr::null_mut();
    // SAFETY: `sid` is a valid SID for the duration of the call; `out` is a
    // valid out-param.
    let ok = unsafe { ConvertSidToStringSidW(sid, &mut out) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `out` was just allocated by `ConvertSidToStringSidW`.
    let s = unsafe { wide_ptr_to_string(out) };
    // SAFETY: freed exactly once, per the Win32 contract for this call.
    unsafe {
        LocalFree(out as *mut _);
    }
    Ok(s)
}

/// Owned `PSID` from [`CreateAppContainerProfile`] /
/// [`DeriveAppContainerSidFromAppContainerName`], freed with `FreeSid` on
/// drop — the pairing those two APIs document, distinct from the
/// `LocalFree`-owned capability SIDs below.
struct OwnedContainerSid(PSID);

impl Drop for OwnedContainerSid {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is a SID from `CreateAppContainerProfile` or
            // `DeriveAppContainerSidFromAppContainerName`, both of which
            // document `FreeSid` as the release call.
            unsafe {
                FreeSid(self.0);
            }
        }
    }
}

/// A registered AppContainer profile: one per distinct fs projection of a
/// shell session, created the first time that projection becomes
/// enforceable and deleted at session end.
pub(crate) struct AppContainerProfile {
    name: String,
    sid: OwnedContainerSid,
}

impl AppContainerProfile {
    // after mxc appcontainer_runner.rs::AppContainerScriptRunner::create_app_container_sid (0e7c3dd)
    /// Create the AppContainer profile named `name`, or — when a prior
    /// session of the same name crashed before deleting it — reuse it by
    /// deriving its SID from the name instead of failing. `name` must be a
    /// valid AppContainer profile name (non-empty, at most 64 UTF-16 code
    /// units); deriving that name from the shell session id is the caller's
    /// job, not this function's.
    pub(crate) fn create_or_reuse(name: &str) -> io::Result<Self> {
        let name_wide = to_wide(name);
        let display_wide = to_wide("ral sandboxed command");
        let desc_wide =
            to_wide("AppContainer profile for one fs projection of a ral shell session");

        let mut sid: PSID = ptr::null_mut();
        // SAFETY: the three wide strings are valid null-terminated UTF-16
        // and outlive the call; `sid` is a valid out-param; no capability
        // list is passed at profile-creation time (capabilities are
        // supplied per-spawn via `CapabilitySids`, not baked into the
        // profile).
        let hr = unsafe {
            CreateAppContainerProfile(
                name_wide.as_ptr(),
                display_wide.as_ptr(),
                desc_wide.as_ptr(),
                ptr::null(),
                0,
                &mut sid,
            )
        };

        if hr == hresult_from_win32(ERROR_ALREADY_EXISTS) {
            // A crashed prior session left this profile registered. MXC
            // treats this as success and re-derives the SID from the name
            // rather than failing, so a stale profile from an earlier crash
            // is transparently reused instead of blocking every future
            // session with the same name.
            let mut derived: PSID = ptr::null_mut();
            // SAFETY: `name_wide` is valid for the duration of the call;
            // `derived` is a valid out-param.
            let hr2 =
                unsafe { DeriveAppContainerSidFromAppContainerName(name_wide.as_ptr(), &mut derived) };
            if hr2 < 0 {
                return Err(hresult_err("DeriveAppContainerSidFromAppContainerName", hr2));
            }
            sid = derived;
        } else if hr < 0 {
            return Err(hresult_err("CreateAppContainerProfile", hr));
        }

        Ok(Self {
            name: name.to_string(),
            sid: OwnedContainerSid(sid),
        })
    }

    /// Borrow the profile's `PSID` for building `SECURITY_CAPABILITIES` or
    /// targeting an ACE at this principal. Valid only while `self` is alive.
    pub(crate) fn sid(&self) -> PSID {
        self.sid.0
    }

    /// The profile's registered name, for ledger bookkeeping.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The profile's SID in `S-1-15-...` string form, for ACE targeting and
    /// session ledger keys.
    pub(crate) fn sid_string(&self) -> io::Result<String> {
        sid_to_string(self.sid.0)
    }

    // after mxc appcontainer_runner.rs::delete_app_container_profile (0e7c3dd)
    /// Delete the profile at session end. Consumes `self`: the in-memory SID
    /// is freed via the normal `Drop` regardless of whether the OS-level
    /// delete succeeds, since there is nothing further to do with either
    /// once this returns.
    pub(crate) fn delete(self) -> io::Result<()> {
        delete_profile_by_name(&self.name)
    }
}

// after mxc appcontainer_runner.rs::delete_app_container_profile (0e7c3dd)
/// Delete the AppContainer profile registered as `name`, by name alone —
/// the orphan-recovery path, where no live [`AppContainerProfile`] value
/// exists for a crashed session's ledgered profile. The ledger is written
/// before the OS-level create, so a recorded name may never have been
/// registered; the caller tolerates failure here rather than this function
/// guessing which HRESULT means "was never there".
pub(crate) fn delete_profile_by_name(name: &str) -> io::Result<()> {
    let name_wide = to_wide(name);
    // SAFETY: `name_wide` is a valid null-terminated UTF-16 string for
    // the duration of the call.
    let hr = unsafe { DeleteAppContainerProfile(name_wide.as_ptr()) };
    if hr < 0 {
        return Err(hresult_err("DeleteAppContainerProfile", hr));
    }
    Ok(())
}

// after mxc appcontainer_runner.rs::OwnedCapabilitySid (0e7c3dd)
/// Owned capability `PSID` from `DeriveCapabilitySidsFromName`, freed with
/// `LocalFree` on drop — that API's SIDs are `LocalAlloc`'d, unlike the
/// `FreeSid`-owned container SID above.
struct OwnedCapabilitySid(PSID);

impl OwnedCapabilitySid {
    /// Derive the capability SID for `name` (e.g. `internetClient`). Frees
    /// every SID array `DeriveCapabilitySidsFromName` returns except the one
    /// capability SID kept: the group SIDs (unused here) and any capability
    /// SIDs past the first (a capability name can in principle map to more
    /// than one; MXC keeps only the first and so do we).
    fn from_capability_name(name: &str) -> io::Result<Self> {
        let wide_name = to_wide(name);

        let mut capability_sids: *mut PSID = ptr::null_mut();
        let mut capability_sid_count: u32 = 0;
        let mut group_sids: *mut PSID = ptr::null_mut();
        let mut group_sid_count: u32 = 0;

        // SAFETY: `DeriveCapabilitySidsFromName` writes `LocalAlloc`'d SID
        // arrays and counts on success; we read only within those counts
        // and free every returned pointer or array exactly once.
        unsafe {
            let ok = DeriveCapabilitySidsFromName(
                wide_name.as_ptr(),
                &mut group_sids,
                &mut group_sid_count,
                &mut capability_sids,
                &mut capability_sid_count,
            );
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }

            for i in 0..group_sid_count {
                let sid = *group_sids.add(i as usize);
                LocalFree(sid);
            }
            LocalFree(group_sids as *mut _);

            if capability_sid_count == 0 {
                LocalFree(capability_sids as *mut _);
                return Err(io::Error::other(format!(
                    "no capability SID returned for '{name}'"
                )));
            }

            let result_sid = *capability_sids;
            for i in 1..capability_sid_count {
                let sid = *capability_sids.add(i as usize);
                LocalFree(sid);
            }
            LocalFree(capability_sids as *mut _);

            Ok(Self(result_sid))
        }
    }
}

impl Drop for OwnedCapabilitySid {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is a `LocalAlloc`'d SID from
            // `DeriveCapabilitySidsFromName`, released with `LocalFree`.
            unsafe {
                LocalFree(self.0 as *mut _);
            }
        }
    }
}

/// The two well-known capability names MXC's Tier 3 grants for outbound
/// network access (`appcontainer_runner.rs::spawn_suspended`,
/// `mxc-sdk/src/policy.rs::apply_backend`, both 0e7c3dd).
const NETWORK_CAPABILITIES: [&str; 2] = ["internetClient", "privateNetworkClientServer"];

/// The capability-SID array for a confined spawn's `SECURITY_CAPABILITIES`.
///
/// Owns the derived capability SIDs (freed on drop) alongside the
/// `SID_AND_ATTRIBUTES` view over them that
/// [`crate::process::launch::Launch::security_capabilities`] copies from —
/// the owner must outlive that call, since the copied entries still point at
/// this struct's `LocalAlloc`'d memory.
pub(crate) struct CapabilitySids {
    _owned: Vec<OwnedCapabilitySid>,
    entries: Vec<SID_AND_ATTRIBUTES>,
}

impl CapabilitySids {
    // after mxc appcontainer_runner.rs::AppContainerScriptRunner::spawn_suspended
    // (capability-SID derivation loop, 0e7c3dd)
    /// Build the capability array: the network capabilities when
    /// `allow_network` is set, empty otherwise. An empty array is not a
    /// degenerate case — the withheld capabilities *are* the entire
    /// enforcement of a `net: false` projection, since an AppContainer with
    /// neither capability cannot open a socket.
    ///
    /// Unlike upstream, which logs a warning and continues when a
    /// capability-SID derivation fails, this fails the whole build: silently
    /// under-granting a capability the caller explicitly requested is a
    /// correctness bug worth surfacing, not a warning, and both capability
    /// names here are well-known ones present on any supported Windows
    /// version.
    pub(crate) fn build(allow_network: bool) -> io::Result<Self> {
        let mut owned = Vec::new();
        let mut entries = Vec::new();

        if allow_network {
            for name in NETWORK_CAPABILITIES {
                let sid = OwnedCapabilitySid::from_capability_name(name)?;
                entries.push(SID_AND_ATTRIBUTES {
                    Sid: sid.0,
                    Attributes: SE_GROUP_ENABLED as u32,
                });
                owned.push(sid);
            }
        }

        Ok(Self {
            _owned: owned,
            entries,
        })
    }

    /// The `SID_AND_ATTRIBUTES` view to pass to
    /// [`crate::process::launch::Launch::security_capabilities`].
    pub(crate) fn entries(&self) -> &[SID_AND_ATTRIBUTES] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hresult_from_win32_matches_already_exists_constant() {
        // ERROR_ALREADY_EXISTS (183) wrapped in FACILITY_WIN32 is the
        // well-known 0x800700B7 -- verifies the reproduced macro against a
        // value anyone can check against `winerror.h`.
        assert_eq!(hresult_from_win32(ERROR_ALREADY_EXISTS) as u32, 0x8007_00B7);
    }

    #[test]
    fn capability_sids_empty_when_network_denied() {
        // `net: false` semantics: nothing derived, nothing to free.
        let caps = CapabilitySids::build(false).expect("build never touches the OS when empty");
        assert!(caps.entries().is_empty());
    }

    #[test]
    fn capability_sids_derives_both_network_capabilities_when_allowed() {
        let caps = CapabilitySids::build(true).expect(
            "internetClient/privateNetworkClientServer are well-known capabilities on any \
             supported Windows version",
        );
        assert_eq!(caps.entries().len(), NETWORK_CAPABILITIES.len());
        for entry in caps.entries() {
            assert!(!entry.Sid.is_null());
            assert_eq!(entry.Attributes, SE_GROUP_ENABLED as u32);
        }
    }

    #[test]
    fn profile_create_reuse_sid_string_and_delete_round_trip() {
        // A name unique enough not to collide with a real prior run or a
        // concurrent test process.
        let name = format!("ral.test.appcontainer.{}", std::process::id());

        // First create: the plain `CreateAppContainerProfile` success path.
        let profile = AppContainerProfile::create_or_reuse(&name).expect("first create succeeds");
        assert_eq!(profile.name, name);
        assert!(!profile.sid().is_null());
        let sid_string = profile.sid_string().expect("SID converts to string form");
        assert!(
            sid_string.starts_with("S-1-15-"),
            "unexpected AppContainer SID: {sid_string}"
        );

        // Second create while the profile still exists on disk: exercises
        // the already-exists/reuse fallback. Same name derives the same
        // SID. Drop the first handle first (frees only the in-memory SID,
        // not the OS profile) so the only live owner is `reused` when it
        // deletes the OS-level profile below.
        drop(profile);
        let reused = AppContainerProfile::create_or_reuse(&name).expect("reuse succeeds");
        assert_eq!(
            reused.sid_string().expect("SID converts to string form"),
            sid_string
        );
        reused.delete().expect("delete succeeds");

        // Re-create after delete: exercises the plain create path again,
        // and the same SID string must come back since it is a
        // deterministic function of the name.
        let recreated = AppContainerProfile::create_or_reuse(&name).expect("re-create succeeds");
        assert_eq!(
            recreated.sid_string().expect("SID converts to string form"),
            sid_string
        );
        recreated.delete().expect("final delete succeeds");
    }
}
