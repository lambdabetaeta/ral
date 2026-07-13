//! Windows sandbox support.
//!
//! - [`apply_job_limits`] — post-spawn Job Object capping a single
//!   external command's process tree at [`super::ACTIVE_PROCESS_CAP`]
//!   processes.  Used for plain externals inside a grant; gives
//!   fork-bomb mitigation, nothing more.  There is a brief window
//!   between `CreateProcess` and `AssignProcessToJobObject` during which
//!   the child is unconstrained — acceptable for this use case.
//! - [`dump_profile_for_windows`] — renders what the AppContainer backend
//!   actually enforces for a projection, for `RAL_DUMP_SANDBOX_PROFILE`.
//!
//! This module owns the Job Object plumbing and the profile dump.  Its
//! submodules carry the AppContainer backend: `appcontainer` (profile
//! lifecycle + LowBox spawn capabilities), `dacl` (grant-ACE apply/restore
//! engine) — both imitating MXC's Tier-3 processcontainer backend
//! (github.com/microsoft/mxc @ 0e7c3dd) — and `session`, which composes the
//! two into projection-keyed confinement: one profile per distinct fs
//! projection, held in session state until teardown.

pub(crate) mod appcontainer;
pub(crate) mod dacl;
pub(crate) mod session;

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

/// Render what the AppContainer backend enforces for `policy`, for
/// `RAL_DUMP_SANDBOX_PROFILE`.  Unlike the Seatbelt SBPL / bwrap argv dumps
/// this describes real enforcement: the LowBox profile every confined child
/// spawns under, the capability SIDs (network on/off), and the host prefixes
/// stamped with an allow-ACE for the profile SID, read vs read-write.
pub(super) fn dump_profile_for_windows(policy: &SandboxProjection) -> String {
    let mut out = String::new();
    out.push_str("AppContainer (LowBox token) profile:\n");
    out.push_str(
        "  profile: one per distinct fs projection of the session\n\
         \x20     (name ral.sandbox.s<pid>.p<n>); this projection's SID sees\n\
         \x20     only the ACEs stamped below, never another projection's\n",
    );
    out.push_str("  fs: deny-by-default; the ALL APPLICATION PACKAGES group's\n");
    out.push_str("      system-path reads plus the allow-ACEs stamped below\n");
    out.push_str(&format!(
        "  net: {} -- {}\n",
        if policy.net { "allow" } else { "deny" },
        if policy.net {
            "internetClient + privateNetworkClientServer capability SIDs granted"
        } else {
            "no network capability SID granted; the LowBox token cannot open a socket"
        }
    ));
    out.push_str(
        "  program image: the confined child's binary is granted RO (read+execute)\n\
             \x20     so the LowBox token can load it (an absolute host path or the\n\
             \x20     bundled-tool ral.exe; a bare-name host program rests on the fs\n\
             \x20     read projection / ALL APPLICATION PACKAGES) -- mirrors the Linux\n\
             \x20     bind of the program path\n",
    );
    out.push_str("  fs allow-ACEs (SID = the profile's AppContainer SID):\n");
    if let Some(fs) = policy.fs.as_policy() {
        out.push_str("    read-write (FILE_GENERIC_READ|WRITE|EXECUTE|DELETE):\n");
        for p in &fs.write_prefixes {
            out.push_str(&format!("      {}\n", p.as_str()));
        }
        out.push_str("    read-only (FILE_GENERIC_READ|EXECUTE):\n");
        for p in &fs.read_prefixes {
            if fs.write_prefixes.iter().any(|w| w == p) {
                continue;
            }
            out.push_str(&format!("      {}\n", p.as_str()));
        }
        if !fs.deny_paths.is_empty() {
            out.push_str(
                "    deny_paths: every one gets an explicit deny-ACE (deny\n\
                     \x20     FILE_ALL_ACCESS), unconditionally -- an AppContainer token\n\
                     \x20     retains Everyone and ALL APPLICATION PACKAGES grants are\n\
                     \x20     system-wide, so a path outside this projection's own grants\n\
                     \x20     is not actually unreachable (skipped only if it does not\n\
                     \x20     exist, or this session already denied it):\n",
            );
            for p in &fs.deny_paths {
                out.push_str(&format!("      {}\n", p.as_str()));
            }
        }
    } else {
        out.push_str(
            "    (fs unrestricted in the projection, but AppContainer is deny-by-default:\n\
                 \x20    no allow-ACE is stamped, so the child reads only system paths)\n",
        );
    }
    out
}
