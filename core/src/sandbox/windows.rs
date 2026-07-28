//! Windows sandbox: the fork-bomb Job Object, and the profile dump.
//!
//! Confinement proper lives in the submodules — `appcontainer` (profile
//! lifecycle and `LowBox` spawn capabilities), `dacl` (grant-ACE apply and
//! restore), and `session`, which composes them into one profile per distinct
//! fs projection, held until session teardown.

pub(crate) mod appcontainer;
pub(crate) mod dacl;
pub(crate) mod session;

use std::fmt::Write as _;

use crate::types::SandboxProjection;
use windows_sys::Win32::Foundation::CloseHandle;

/// Cap a standalone external's process tree at `ACTIVE_PROCESS_CAP` live
/// processes.  Post-spawn, so the child runs unconstrained for the window
/// between `CreateProcess` and `AssignProcessToJobObject`.
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
            // Commonly ral itself sitting in a job that forbids nesting.
            crate::dbg_trace!(
                "sandbox-win",
                "AssignProcessToJobObject failed, fork-bomb cap not applied: {}",
                std::io::Error::last_os_error()
            );
        }
        // The job outlives this handle: it dissolves only once its last member
        // has exited, so the cap holds for the child's whole tree.
        CloseHandle(job);
    }
}

/// Render what the `AppContainer` backend enforces for `policy`, for
/// `RAL_DUMP_SANDBOX_PROFILE`.  Enforcement here is a token plus stamped ACEs,
/// not a printable artifact like the Seatbelt profile or the bwrap argv.
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
    let _ = writeln!(
        out,
        "  net: {} -- {}",
        if policy.net { "allow" } else { "deny" },
        if policy.net {
            "internetClient + privateNetworkClientServer capability SIDs granted"
        } else {
            "no network capability SID granted; the LowBox token cannot open a socket"
        }
    );
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
            let _ = writeln!(out, "      {}", p.as_str());
        }
        out.push_str("    read-only (FILE_GENERIC_READ|EXECUTE):\n");
        for p in &fs.read_prefixes {
            if fs.write_prefixes.iter().any(|w| w == p) {
                continue;
            }
            let _ = writeln!(out, "      {}", p.as_str());
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
                let _ = writeln!(out, "      {}", p.as_str());
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
