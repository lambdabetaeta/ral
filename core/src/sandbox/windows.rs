//! Windows sandbox support.
//!
//! - [`apply_job_limits`] — post-spawn Job Object capping a single
//!   external command's process tree at [`super::ACTIVE_PROCESS_CAP`]
//!   processes.  Used for plain externals inside a grant; gives
//!   fork-bomb mitigation, nothing more.  There is a brief window
//!   between `CreateProcess` and `AssignProcessToJobObject` during which
//!   the child is unconstrained — acceptable for this use case.
//! - [`dump_profile_for_windows`] — audit-only renderer that describes
//!   the profile this backend would install, for
//!   `RAL_DUMP_SANDBOX_PROFILE`.  It enforces nothing.
//!
//! This module owns the Job Object plumbing and the profile-dump
//! advisory.  Its submodules carry the per-command AppContainer
//! backend: `appcontainer` (profile lifecycle + LowBox spawn
//! capabilities) and `dacl` (grant-ACE apply/restore engine), both
//! imitating MXC's Tier-3 processcontainer backend
//! (github.com/microsoft/mxc @ 0e7c3dd).

pub(crate) mod appcontainer;
pub(crate) mod dacl;

use crate::types::SandboxProjection;
use windows_sys::Win32::Foundation::CloseHandle;

pub(super) fn apply_job_limits(child: &crate::process::ChildHandle) {
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        if !crate::process::set_active_process_limit(job, super::ACTIVE_PROCESS_CAP) {
            CloseHandle(job);
            return;
        }
        let proc_handle = child.raw_process_handle();
        if AssignProcessToJobObject(job, proc_handle) == 0 {
            // The active-process cap silently does not apply — commonly
            // when ral itself runs inside a job that disallows nesting.
            // This path returns (), so trace rather than propagate.
            crate::dbg_trace!(
                "sandbox-win",
                "AssignProcessToJobObject failed, fork-bomb cap not applied: {}",
                std::io::Error::last_os_error()
            );
        }
        CloseHandle(job);
    }
}

/// Render a human-readable dump of the profile this backend would
/// install for `policy`, for `RAL_DUMP_SANDBOX_PROFILE`.  Audit only: it
/// enforces nothing, and mirrors the Seatbelt SBPL / bwrap argv dumps
/// for the Win32 path.
pub(super) fn dump_profile_for_windows(policy: &SandboxProjection) -> String {
    let mut out = String::new();
    out.push_str("Restricted-token plan:\n");
    out.push_str("  integrity level: Low (S-1-16-4096)\n");
    out.push_str("  restricting SIDs: [S-1-5-12 RESTRICTED]\n");
    out.push_str(
        "  scratch dir: per-spawn under std::env::temp_dir() as \
ral-sandbox-<pid>-<ns>; stamped Low via SACL\n",
    );
    out.push_str("  mitigation flags:\n");
    out.push_str("    DEP_ENABLE\n");
    out.push_str("    BOTTOM_UP_ASLR_ALWAYS_ON\n");
    out.push_str("    HEAP_TERMINATE_ALWAYS_ON\n");
    out.push_str("    BLOCK_NON_MICROSOFT_BINARIES_ALWAYS_ON\n");
    out.push_str(&format!(
        "  net: {} -- this backend has no kernel network enforcement; a \
net-restricting projection (net: false) is rejected fail-closed at command \
dispatch (see projection_enforceable), not here, so this audit dump renders \
whatever projection it is handed\n",
        if policy.net { "allow" } else { "deny" }
    ));
    out.push_str("  ignored (advisory under this backend):\n");
    if let Some(fs) = policy.fs.as_policy() {
        out.push_str("    fs.read_prefixes (logged only, not enforced as allow-list):\n");
        for p in &fs.read_prefixes {
            out.push_str(&format!("      {}\n", p.as_str()));
        }
        out.push_str(
            "    fs.write_prefixes (logged only, not enforced; only scratch dir writable):\n",
        );
        for p in &fs.write_prefixes {
            out.push_str(&format!("      {}\n", p.as_str()));
        }
        out.push_str("    fs.deny_paths (logged only, not enforced):\n");
        for p in &fs.deny_paths {
            out.push_str(&format!("      {}\n", p.as_str()));
        }
    } else {
        out.push_str("    fs: unrestricted (Low IL still blocks writes to Medium+ objects)\n");
    }
    out
}
