//! The macOS fork brake: the `RLIMIT_NPROC` a confined child is launched with.
//!
//! The limit is an absolute count of the user's processes, so the budget is
//! measured at launch: the user's own session growing past it makes the
//! confined command's forks fail first, never the desktop's.

use std::io;

use super::ACTIVE_PROCESS_CAP;

/// `PROC_RUID_ONLY` from `<libproc.h>`, which the `libc` crate does not export:
/// the kernel counts processes per real UID.
const PROC_RUID_ONLY: u32 = 5;

/// The child's `RLIMIT_NPROC`: the user's live processes now, plus the budget.
pub(super) fn limit() -> io::Result<libc::rlim_t> {
    let ceiling = per_user_ceiling()?;
    Ok(budgeted(
        live_processes(ceiling)?,
        libc::rlim_t::from(ACTIVE_PROCESS_CAP),
        ceiling,
    ))
}

fn budgeted(live: libc::rlim_t, budget: libc::rlim_t, ceiling: libc::rlim_t) -> libc::rlim_t {
    live.saturating_add(budget).min(ceiling)
}

fn per_user_ceiling() -> io::Result<libc::rlim_t> {
    let mut ceiling: libc::c_int = 0;
    let mut len = size_of_val(&ceiling);
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.maxprocperuid".as_ptr(),
            (&raw mut ceiling).cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    libc::rlim_t::try_from(ceiling).map_err(io::Error::other)
}

/// The user's live processes, read from the filled list: a null buffer answers
/// with a system-wide estimate that runs well above the user's own count.
/// `capacity` is the most the kernel lets one user hold.
fn live_processes(capacity: libc::rlim_t) -> io::Result<libc::rlim_t> {
    let mut pids = vec![0; usize::try_from(capacity).map_err(io::Error::other)?];
    let bytes = libc::c_int::try_from(size_of_val(pids.as_slice())).map_err(io::Error::other)?;
    let filled = unsafe {
        libc::proc_listpids(
            PROC_RUID_ONLY,
            libc::getuid(),
            pids.as_mut_ptr().cast(),
            bytes,
        )
    };
    // Zero is its error return, and a live user has at least this process.
    if filled <= 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(libc::rlim_t::from(filled.unsigned_abs()) / size_of::<libc::pid_t>() as libc::rlim_t)
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_rides_above_the_live_count_up_to_the_ceiling() {
        assert_eq!(budgeted(400, 512, 2666), 912);
        assert_eq!(budgeted(2154, 512, 2666), 2666);
        assert_eq!(budgeted(3000, 512, 2666), 2666);
    }

    #[test]
    fn the_limit_is_measured_from_the_users_own_processes() {
        let ceiling = per_user_ceiling().unwrap();
        let live = live_processes(ceiling).unwrap();
        assert!(live >= 1, "this process is the user's");
        let limit = limit().unwrap();
        assert!(
            limit <= ceiling && limit > libc::rlim_t::from(ACTIVE_PROCESS_CAP),
            "limit {limit} outside (budget, ceiling {ceiling}]"
        );
    }

    #[test]
    fn a_limited_child_holds_the_limit_as_soft_and_hard() {
        use crate::process::{Launch, PgidPolicy, StdioSpec};
        use std::io::Read;
        let mut cmd = Launch::new("sh");
        cmd.args(["-c", "ulimit -Su; ulimit -Hu"]);
        cmd.stdout(StdioSpec::piped());
        cmd.limit_resources().unwrap();
        let (mut child, ..) = cmd.spawn(PgidPolicy::Inherit).unwrap();
        let mut out = String::new();
        child
            .take_stdout()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        child.reap().unwrap();
        let limits: Vec<libc::rlim_t> = out.lines().map(|l| l.parse().unwrap()).collect();
        let ceiling = per_user_ceiling().unwrap();
        assert!(
            matches!(*limits, [soft, hard] if soft == hard && soft <= ceiling),
            "soft and hard must agree within the ceiling {ceiling}: {limits:?}"
        );
    }
}
