// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// Ported from `appcontainer_runner.rs` and `wxc_common/string_util.rs` of
// github.com/microsoft/mxc @ 0e7c3dd, adapted to `windows-sys`; each ported
// unit carries an `after mxc` breadcrumb naming its upstream counterpart.

//! `AppContainer` profile lifecycle and `LowBox` spawn capabilities.
//!
//! One profile per distinct fs projection a shell session confines. `session`
//! owns the keying, naming and lifetime, `dacl` owns the ACE stamping that
//! gives a profile's SID its filesystem reach, and this module builds only the
//! profile and the `SECURITY_CAPABILITIES` a confined spawn hands to
//! `Launch::security_capabilities`. Withholding the network capabilities is
//! the whole of `net: false`: an `AppContainer` holding neither
//! `internetClient` nor `privateNetworkClientServer` cannot open a socket.
//!
//! Upstream can opt a child out of `ALL APPLICATION PACKAGES` grants (LPAC)
//! and disable Win32k syscalls; we do neither. Both assume a known, packaged,
//! headless binary, and our children are arbitrary host tools that read
//! package-readable system paths and touch window-station API incidentally.

use std::io;
use std::ptr;

use windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS;
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    DeriveCapabilitySidsFromName, FreeSid, PSID, SID_AND_ATTRIBUTES,
};
use windows_sys::Win32::System::SystemServices::SE_GROUP_ENABLED;

// `windows-sys` aliases `PSID` and `HLOCAL` to the same `*mut c_void`, so a
// `PSID` goes straight into `LocalFree` uncast.
use windows_sys::Win32::Foundation::LocalFree;

/// `HRESULT_FROM_WIN32` from `winerror.h`, which `windows-sys` cannot
/// generate because it is a header-only macro. Every input here is a genuine
/// Win32 error code, so the full macro's "already an HRESULT" branch is absent.
const fn hresult_from_win32(code: u32) -> i32 {
    ((code & 0x0000_FFFF) | (0x7 << 16) | 0x8000_0000).cast_signed()
}

fn hresult_err(context: &str, hr: i32) -> io::Error {
    io::Error::other(format!(
        "{context} failed: hresult=0x{:08X}",
        hr.cast_unsigned()
    ))
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Copy a null-terminated UTF-16 string out of OS memory the caller frees.
///
/// # Safety
/// `ptr` must point at a null-terminated UTF-16 string.
unsafe fn wide_ptr_to_string(ptr: *const u16) -> String {
    let mut len = 0usize;
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    String::from_utf16_lossy(slice)
}

// after mxc wxc_common/string_util.rs::sid_to_string
/// Render a `PSID` in `S-1-…` form, freeing the OS-allocated output string.
fn sid_to_string(sid: PSID) -> io::Result<String> {
    let mut out: *mut u16 = ptr::null_mut();
    // SAFETY: `sid` is valid for the call; `out` is a valid out-param.
    let ok = unsafe { ConvertSidToStringSidW(sid, &raw mut out) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `out` was just allocated by `ConvertSidToStringSidW`.
    let s = unsafe { wide_ptr_to_string(out) };
    // SAFETY: freed exactly once, as that call's contract requires.
    unsafe {
        LocalFree(out.cast());
    }
    Ok(s)
}

/// Owned `PSID` from `CreateAppContainerProfile` or
/// `DeriveAppContainerSidFromAppContainerName`, whose documented release is
/// `FreeSid` — not the `LocalFree` the capability SIDs below take.
struct OwnedContainerSid(PSID);

impl Drop for OwnedContainerSid {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `FreeSid` is the release call for both producers.
            unsafe {
                FreeSid(self.0);
            }
        }
    }
}

/// A registered `AppContainer` profile: live from its projection's first
/// enforceable command to session end.
pub(crate) struct AppContainerProfile {
    name: String,
    sid: OwnedContainerSid,
}

impl AppContainerProfile {
    // after mxc appcontainer_runner.rs::AppContainerScriptRunner::create_app_container_sid
    /// Register the profile named `name`, or adopt one a crashed prior
    /// session left behind by deriving its SID from the name. `name` must be
    /// a valid `AppContainer` profile name — non-empty, at most 64 UTF-16
    /// code units — which `session` mints from the shell session id.
    pub(crate) fn create_or_reuse(name: &str) -> io::Result<Self> {
        let name_wide = to_wide(name);
        let display_wide = to_wide("ral sandboxed command");
        let desc_wide =
            to_wide("AppContainer profile for one fs projection of a ral shell session");

        let mut sid: PSID = ptr::null_mut();
        // SAFETY: the wide strings are null-terminated and outlive the call;
        // `sid` is a valid out-param. The null capability list is deliberate
        // — capabilities are per-spawn, never baked into the profile.
        let hr = unsafe {
            CreateAppContainerProfile(
                name_wide.as_ptr(),
                display_wide.as_ptr(),
                desc_wide.as_ptr(),
                ptr::null(),
                0,
                &raw mut sid,
            )
        };

        if hr == hresult_from_win32(ERROR_ALREADY_EXISTS) {
            // A crashed prior session left this registered. Deriving the SID
            // from the name adopts it, rather than blocking every future
            // session that picks the same name.
            let mut derived: PSID = ptr::null_mut();
            // SAFETY: `name_wide` outlives the call; `derived` is a valid
            // out-param.
            let hr2 = unsafe {
                DeriveAppContainerSidFromAppContainerName(name_wide.as_ptr(), &raw mut derived)
            };
            if hr2 < 0 {
                return Err(hresult_err(
                    "DeriveAppContainerSidFromAppContainerName",
                    hr2,
                ));
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

    /// The profile's `PSID`, valid only while `self` lives.
    pub(crate) fn sid(&self) -> PSID {
        self.sid.0
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The SID in `S-1-15-…` string form — the spelling `dacl` targets ACEs
    /// at and records in its ledger.
    pub(crate) fn sid_string(&self) -> io::Result<String> {
        sid_to_string(self.sid.0)
    }

    // after mxc appcontainer_runner.rs::delete_app_container_profile
    /// Delete the profile at session end. Consumes `self` so the in-memory
    /// SID is freed by `Drop` whether or not the OS-level delete succeeds.
    pub(crate) fn delete(self) -> io::Result<()> {
        delete_profile_by_name(&self.name)
    }
}

// after mxc appcontainer_runner.rs::delete_app_container_profile
/// Delete a registered profile by name alone — `dacl`'s boot sweep reclaiming
/// a crashed session's ledgered names, with no live `AppContainerProfile` to
/// hand. The ledger is written before the OS-level create, so a recorded name
/// may never have been registered; that caller swallows the error rather than
/// this one guessing which HRESULT means "never there".
pub(crate) fn delete_profile_by_name(name: &str) -> io::Result<()> {
    let name_wide = to_wide(name);
    // SAFETY: `name_wide` is null-terminated and outlives the call.
    let hr = unsafe { DeleteAppContainerProfile(name_wide.as_ptr()) };
    if hr < 0 {
        return Err(hresult_err("DeleteAppContainerProfile", hr));
    }
    Ok(())
}

// after mxc appcontainer_runner.rs::OwnedCapabilitySid
/// Owned capability `PSID`: `DeriveCapabilitySidsFromName` `LocalAlloc`s its
/// SIDs, so this one takes `LocalFree`, not the `FreeSid` above.
struct OwnedCapabilitySid(PSID);

impl OwnedCapabilitySid {
    /// Derive the capability SID for `name` (e.g. `internetClient`), freeing
    /// what else the call returns: the group SIDs, and any capability SIDs
    /// past the first — a name may map to several; we keep the first.
    fn from_capability_name(name: &str) -> io::Result<Self> {
        let wide_name = to_wide(name);

        let mut capability_sids: *mut PSID = ptr::null_mut();
        let mut capability_sid_count: u32 = 0;
        let mut group_sids: *mut PSID = ptr::null_mut();
        let mut group_sid_count: u32 = 0;

        // SAFETY: on success the call writes `LocalAlloc`'d SID arrays and
        // their counts; we read only within the counts and free every
        // returned pointer and array exactly once.
        unsafe {
            let ok = DeriveCapabilitySidsFromName(
                wide_name.as_ptr(),
                &raw mut group_sids,
                &raw mut group_sid_count,
                &raw mut capability_sids,
                &raw mut capability_sid_count,
            );
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }

            for i in 0..group_sid_count {
                let sid = *group_sids.add(i as usize);
                LocalFree(sid);
            }
            LocalFree(group_sids.cast());

            if capability_sid_count == 0 {
                LocalFree(capability_sids.cast());
                return Err(io::Error::other(format!(
                    "no capability SID returned for '{name}'"
                )));
            }

            let result_sid = *capability_sids;
            for i in 1..capability_sid_count {
                let sid = *capability_sids.add(i as usize);
                LocalFree(sid);
            }
            LocalFree(capability_sids.cast());

            Ok(Self(result_sid))
        }
    }
}

impl Drop for OwnedCapabilitySid {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: a `LocalAlloc`'d SID from
            // `DeriveCapabilitySidsFromName`.
            unsafe {
                LocalFree(self.0.cast());
            }
        }
    }
}

/// The well-known Windows capability names that permit outbound sockets.
const NETWORK_CAPABILITIES: [&str; 2] = ["internetClient", "privateNetworkClientServer"];

/// The capability-SID array for a confined spawn's `SECURITY_CAPABILITIES`.
///
/// Owns the derived SIDs alongside the `SID_AND_ATTRIBUTES` view over them
/// that [`crate::process::launch::Launch::security_capabilities`] copies:
/// those copies still point into this struct's `LocalAlloc`'d memory, so it
/// must outlive the spawn — `session` holds it until teardown.
pub(crate) struct CapabilitySids {
    _owned: Vec<OwnedCapabilitySid>,
    entries: Vec<SID_AND_ATTRIBUTES>,
}

impl CapabilitySids {
    // after mxc appcontainer_runner.rs::AppContainerScriptRunner::spawn_suspended
    /// The network capabilities when `allow_network`, empty otherwise.
    /// Upstream warns and carries on when a derivation fails; we fail the
    /// build — both names are well known on every supported Windows, so a
    /// failure is real, and under-granting a requested capability silently is
    /// worse than not spawning.
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

    pub(crate) fn entries(&self) -> &[SID_AND_ATTRIBUTES] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hresult_from_win32_matches_already_exists_constant() {
        // 0x800700B7 is the published wrapping, checkable against `winerror.h`.
        assert_eq!(
            hresult_from_win32(ERROR_ALREADY_EXISTS).cast_unsigned(),
            0x8007_00B7
        );
    }

    #[test]
    fn capability_sids_empty_when_network_denied() {
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
        // Keyed on pid so a concurrent test process cannot collide.
        let name = format!("ral.test.appcontainer.{}", std::process::id());

        let profile = AppContainerProfile::create_or_reuse(&name).expect("first create succeeds");
        assert_eq!(profile.name, name);
        assert!(!profile.sid().is_null());
        let sid_string = profile.sid_string().expect("SID converts to string form");
        assert!(
            sid_string.starts_with("S-1-15-"),
            "unexpected AppContainer SID: {sid_string}"
        );

        // Dropping frees the in-memory SID but leaves the OS profile
        // registered, so the next create takes the already-exists path.
        drop(profile);
        let reused = AppContainerProfile::create_or_reuse(&name).expect("reuse succeeds");
        assert_eq!(
            reused.sid_string().expect("SID converts to string form"),
            sid_string
        );
        reused.delete().expect("delete succeeds");

        // The SID is a deterministic function of the name, so it survives a
        // delete and re-create.
        let recreated = AppContainerProfile::create_or_reuse(&name).expect("re-create succeeds");
        assert_eq!(
            recreated.sid_string().expect("SID converts to string form"),
            sid_string
        );
        recreated.delete().expect("final delete succeeds");
    }
}
