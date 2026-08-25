//! The Linux syscall edge of the jail — cgroup files, the uid/gid drop,
//! `NO_NEW_PRIVS`.  [`super::JailPlan`] decides; this file only carries it out.

use super::{JailCgroup, JailPlan};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::Path;

/// `EEXIST` is the ordinary case: the jail root outlives any one exec, and a
/// sibling exec may have raced this one.
fn mkdir_if_missing(path: &Path) -> io::Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Read-increment-write the guest-global jail sequence counter under an
/// exclusive `flock` on the file itself, so every booted engine's
/// [`super::GuestJail`] mints off the one number line and two never collide
/// on a uid or a cgroup name. `O_CREAT` makes the file's first open its
/// birth; the lock is released when `file` drops at the end of this call.
///
/// # Errors
/// Returns the kernel's error for the parent directory's creation, or the
/// file's open, lock, read, or write.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:jail-seq-file] guest-boot/exec-time jail setup: the guest-global uid/cgroup sequence counter, not model turn-time I/O."
)]
pub(crate) fn next_guest_seq(path: &Path) -> io::Result<u64> {
    use std::io::{Read, Seek, SeekFrom, Write};

    if let Some(dir) = path.parent() {
        mkdir_if_missing(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let seq: u64 = text.trim().parse().unwrap_or(0);
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    file.write_all((seq + 1).to_string().as_bytes())?;
    Ok(seq)
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:jail-cgroup-write] Cgroup control-file writes are guest-boot/exec-time jail setup, not model turn-time I/O: the limits are policy the host sets, never data the model reads or writes."
)]
fn write_control(cgroup: &Path, file: &str, value: &str) -> io::Result<()> {
    std::fs::write(cgroup.join(file), value)
}

fn write_cpu_max(cgroup: &Path, quota_pct: u32) -> io::Result<()> {
    const PERIOD_US: u64 = 100_000;
    let quota_us = PERIOD_US * u64::from(quota_pct) / 100;
    write_control(cgroup, "cpu.max", &format!("{quota_us} {PERIOD_US}"))
}

/// What each level of the jail's tree hands down to the next.
const CONTROLLERS: &str = "+memory +pids +cpu";

/// Create `cgroup` and every ancestor it lacks, delegating [`CONTROLLERS`] into
/// each parent before making the child beneath it.
///
/// A controller's files appear in a child only once the parent's
/// `cgroup.subtree_control` enables it, so the chain from the cgroup2 mount
/// down to the leaf must be unbroken: a directory made under a parent that has
/// not delegated yet is a directory with no `memory.max` to write.  Hence the
/// descent — the recursion climbs to the first ancestor that already exists,
/// the mount root at the latest, and enables on the way back down.
///
/// # Errors
/// Returns the kernel's error for any mkdir, and refuses outright when a parent
/// will not delegate: a jail that cannot be given its limits must not run the
/// command uncapped instead.
fn make_delegated(cgroup: &Path) -> io::Result<()> {
    if cgroup.exists() {
        return Ok(());
    }
    let parent = cgroup.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{} has no parent, so it is not inside a cgroup2 mount. Is the jail root the \
                 path this guest actually mounts cgroup2 at?",
                cgroup.display()
            ),
        )
    })?;
    make_delegated(parent)?;
    write_control(parent, "cgroup.subtree_control", CONTROLLERS).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "could not delegate {CONTROLLERS} from {}: {e}. Without them the cgroup below it \
                 has no memory.max or pids.max to write, and the command would run uncapped.",
                parent.display()
            ),
        )
    })?;
    mkdir_if_missing(cgroup)
}

/// Create the per-exec cgroup (and every level above it, lazily), write its
/// limits, and hand back `cgroup.procs` still open: the pre-exec closure may
/// not allocate, so it cannot build that path and open it itself.
///
/// # Errors
/// Returns the kernel's error for any mkdir/write/open along the way.
pub(crate) fn prepare(plan: &JailPlan) -> io::Result<(JailCgroup, OwnedFd)> {
    make_delegated(&plan.cgroup)?;
    write_control(
        &plan.cgroup,
        "memory.max",
        &plan.limits.memory_max.to_string(),
    )?;
    write_control(&plan.cgroup, "pids.max", &plan.limits.pids_max.to_string())?;
    write_cpu_max(&plan.cgroup, plan.limits.cpu_quota_pct)?;
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:jail-cgroup-procs] Opens cgroup.procs to keep across the fork so the pre-exec closure can place the child without a second open post-uid-drop; guest-boot/exec-time jail setup, not model I/O."
    )]
    let procs = std::fs::OpenOptions::new()
        .write(true)
        .open(plan.cgroup.join("cgroup.procs"))?;
    Ok((
        JailCgroup {
            path: plan.cgroup.clone(),
        },
        OwnedFd::from(procs),
    ))
}

/// Format and write a pid without allocating: this runs in the pre-exec
/// closure, where allocation is forbidden.
fn write_pid(fd: RawFd, pid: libc::pid_t) -> io::Result<()> {
    let mut buf = [0u8; 12];
    let mut n = pid;
    let mut i = buf.len();
    loop {
        i -= 1;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "n % 10 is always 0..=9"
        )]
        {
            buf[i] = b'0' + (n % 10) as u8;
        }
        n /= 10;
        if n == 0 {
            break;
        }
    }
    let bytes = &buf[i..];
    let mut written = 0;
    while written < bytes.len() {
        // SAFETY: `bytes` is a valid stack slice for its own length.
        let rc =
            unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        #[allow(clippy::cast_sign_loss, reason = "rc >= 0 is checked above")]
        {
            written += rc as usize;
        }
    }
    Ok(())
}

/// Place the child in its cgroup and drop it to the plan's uid/gid before
/// `NO_NEW_PRIVS` shuts the door on regaining any of it.  `setresgid` comes
/// first: a process cannot change its gid once its uid is unprivileged.
///
/// # Safety
/// The closure runs between `fork` and `exec`, so every step is
/// async-signal-safe, allocates nothing, and takes no lock.
pub(crate) fn install_pre_exec(
    cmd: &mut std::process::Command,
    plan: &JailPlan,
    cgroup_procs: OwnedFd,
) {
    use std::os::unix::process::CommandExt;
    let uid = plan.uid;
    let gid = plan.gid;
    let procs_fd = cgroup_procs.as_raw_fd();
    unsafe {
        cmd.pre_exec(move || {
            let _keep_open = &cgroup_procs;
            write_pid(procs_fd, libc::getpid())?;
            if libc::setgroups(0, std::ptr::null()) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::setresgid(gid, gid, gid) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::setresuid(uid, uid, uid) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Best-effort, the same stance as `Pgid::signal_group`: a write that fails
/// leaves nothing further to try, and the kernel already did what it would.
pub(crate) fn kill(cgroup: &JailCgroup) {
    let _ = write_control(&cgroup.path, "cgroup.kill", "1");
}

/// `cgroup.kill` returns before its members die, so `rmdir` can lose the race
/// with `EBUSY`: poll briefly, then abandon.  A leftover directory is inert.
pub(crate) fn remove(cgroup: &JailCgroup) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    loop {
        match std::fs::remove_dir(&cgroup.path) {
            Err(e)
                if e.raw_os_error() == Some(libc::EBUSY)
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => return,
        }
    }
}
