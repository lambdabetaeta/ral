//! The jail's Linux edge: cgroups, network isolation, identity and seccomp.

use super::{Count, JailCgroup, JailPlan, Seq, exhausted};
#[cfg(seccomp)]
use crate::authority::{policy::Net, sandbox::Programs};
use crate::process::site::Silent;
use crate::site::site;
use guest_abi::layout::EXEC_CONTROLLERS;
use std::io;
use std::os::fd::OwnedFd;
#[cfg(seccomp)]
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;

#[cfg(seccomp)]
#[allow(clippy::cast_possible_wrap, reason = "ASCII fits c_char")]
const LO: [libc::c_char; 2] = [b'l' as libc::c_char, b'o' as libc::c_char];
#[cfg(seccomp)]
#[allow(clippy::cast_possible_truncation, reason = "IFF_UP is 1")]
const IFF_UP: libc::c_short = libc::IFF_UP as libc::c_short;
#[cfg(seccomp)]
#[allow(
    clippy::cast_possible_truncation,
    reason = "0x8914 fits the ioctl request type"
)]
const SIOCSIFFLAGS: libc::Ioctl = libc::SIOCSIFFLAGS as libc::Ioctl;

/// `EEXIST` is the ordinary case: the jail root outlives any one exec, and a
/// sibling exec may have raced this one.
fn mkdir_if_missing(path: &Path) -> io::Result<()> {
    match site!(Silent::JailCgroupMake, std::fs::create_dir(path)) {
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
pub(crate) fn next_guest_seq(path: &Path) -> io::Result<Seq> {
    use std::io::Read;
    use std::os::unix::fs::FileExt;

    if let Some(dir) = crate::path::spelled_parent(path) {
        mkdir_if_missing(dir)?;
    }
    let mut file = site!(
        Silent::JailSeqFile,
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true) => open(path)
    )?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)?;
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)?;
    let count = Count::decode(&contents).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} holds {contents:?}, not a count of jailed commands; \
                 restarting the guest resets it, since it lives in /run",
                crate::path::shown(path)
            ),
        )
    })?;
    let (seq, next) = count.take().ok_or_else(exhausted)?;
    file.write_all_at(&next.encode(), 0)?;
    Ok(seq)
}

fn write_control(cgroup: &Path, file: &str, value: &str) -> io::Result<()> {
    site!(
        Silent::JailCgroupWrite,
        std::fs::write(cgroup.join(file), value)
    )
}

fn write_cpu_max(cgroup: &Path, quota_pct: u32) -> io::Result<()> {
    const PERIOD_US: u64 = 100_000;
    let quota_us = PERIOD_US * u64::from(quota_pct) / 100;
    write_control(cgroup, "cpu.max", &format!("{quota_us} {PERIOD_US}"))
}

/// Create `cgroup` and every ancestor it lacks, delegating [`EXEC_CONTROLLERS`] into
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
    if site!(Silent::JailCgroupProbe, cgroup => exists()) {
        return Ok(());
    }
    let parent = crate::path::spelled_parent(cgroup).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{} has no parent, so it is not inside a cgroup2 mount. Is the jail root the \
                 path this guest actually mounts cgroup2 at?",
                crate::path::shown(cgroup)
            ),
        )
    })?;
    make_delegated(parent)?;
    write_control(parent, "cgroup.subtree_control", EXEC_CONTROLLERS).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "could not delegate {EXEC_CONTROLLERS} from {}: {e}. Without them the cgroup below it \
                 has no memory.max or pids.max to write, and the command would run uncapped.",
                crate::path::shown(parent)
            ),
        )
    })?;
    mkdir_if_missing(cgroup)
}

/// Create the per-exec cgroup (and every level above it, lazily), write its
/// limits, and hand back `cgroup.procs` still open: the pre-exec closure may
/// not allocate, so it cannot build that path and open it itself.  The cgroup
/// is owned from its `mkdir`, so a failure after it finishes it.
///
/// # Errors
/// Returns the kernel's error for any mkdir/write/open along the way.
pub(crate) fn prepare(plan: &JailPlan) -> io::Result<(JailCgroup, OwnedFd)> {
    make_delegated(&plan.cgroup)?;
    let cgroup = JailCgroup {
        path: plan.cgroup.clone(),
    };
    write_control(
        &plan.cgroup,
        "memory.max",
        &plan.limits.memory_max.to_string(),
    )?;
    write_control(&plan.cgroup, "pids.max", &plan.limits.pids_max.to_string())?;
    write_cpu_max(&plan.cgroup, plan.limits.cpu_quota_pct)?;
    let procs = site!(
        Silent::JailCgroupProcs,
        std::fs::OpenOptions::new().write(true) => open(plan.cgroup.join("cgroup.procs"))
    )?;
    Ok((cgroup, OwnedFd::from(procs)))
}

/// Format and write a pid without allocating: this runs in the pre-exec
/// closure, where allocation is forbidden.
#[cfg(seccomp)]
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
/// `NO_NEW_PRIVS` forbids regaining any of it.  `setresgid` comes
/// first: a process cannot change its gid once its uid is unprivileged.
///
/// # Safety
/// The closure runs between `fork` and `exec`, so every step is
/// async-signal-safe, allocates nothing, and takes no lock.
#[cfg(seccomp)]
pub(crate) fn install_pre_exec(
    cmd: &mut std::process::Command,
    plan: &JailPlan,
    cgroup_procs: OwnedFd,
    deny: &'static Programs,
) {
    use std::os::unix::process::CommandExt;
    let uid = plan.seq.uid();
    let gid = uid;
    let net = plan.net;
    let procs_fd = cgroup_procs.as_raw_fd();
    unsafe {
        cmd.pre_exec(move || {
            let _keep_open = &cgroup_procs;
            write_pid(procs_fd, libc::getpid())?;
            match net {
                Some(Net::Offline) => isolate_network()?,
                None => (),
            }
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
            mark_descriptors()?;
            deny.install()
        });
    }
}

#[cfg(seccomp)]
fn isolate_network() -> io::Result<()> {
    // SAFETY: all storage is on the stack; the socket belongs to this call.
    unsafe {
        if libc::unshare(libc::CLONE_NEWNET) < 0 {
            return Err(io::Error::last_os_error());
        }
        let socket = site!(
            Silent::JailLoopbackSocket,
            libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0)
        );
        if socket < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut interface: libc::ifreq = std::mem::zeroed();
        interface.ifr_name[..LO.len()].copy_from_slice(&LO);
        interface.ifr_ifru.ifru_flags = IFF_UP;
        let result = libc::ioctl(socket, SIOCSIFFLAGS, &raw const interface);
        let error = (result < 0).then(io::Error::last_os_error);
        libc::close(socket);
        error.map_or(Ok(()), Err)
    }
}

#[cfg(seccomp)]
fn mark_descriptors() -> io::Result<()> {
    // SAFETY: close_range marks descriptors without accessing userspace memory.
    let result = unsafe {
        site!(
            Silent::JailDescriptorMark,
            libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            )
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
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
        match site!(Silent::JailCgroupRemove, std::fs::remove_dir(&cgroup.path)) {
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

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::process::jail::SPAN;

    #[test]
    fn the_sequence_file_keeps_its_width_and_refuses_corruption() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("seq");
        std::fs::write(&path, b"0000000009").unwrap();
        assert_eq!(next_guest_seq(&path).unwrap(), Seq::new(9).unwrap());
        assert_eq!(next_guest_seq(&path).unwrap(), Seq::new(10).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"0000000011");
        std::fs::write(&path, b"abc").unwrap();
        let error = next_guest_seq(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("not a count of jailed commands"));
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        std::fs::write(&path, Count(SPAN).encode()).unwrap();
        assert!(
            next_guest_seq(&path)
                .unwrap_err()
                .to_string()
                .contains("every uid")
        );
        assert_eq!(std::fs::read(&path).unwrap(), Count(SPAN).encode());
    }

    #[cfg(seccomp)]
    mod confinement {
        use super::*;
        use crate::authority::sandbox::in_child;
        use std::os::unix::process::CommandExt;

        fn checked(result: libc::c_int) -> io::Result<libc::c_int> {
            if result < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(result)
            }
        }

        #[test]
        fn descriptors_above_stdio_are_marked_close_on_exec() {
            use std::os::fd::FromRawFd;

            // SAFETY: the path is static; the returned descriptor is owned here.
            let fd = checked(unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) }).unwrap();
            // SAFETY: open returned this fresh descriptor.
            let file = unsafe { OwnedFd::from_raw_fd(fd) };
            let fd = file.as_raw_fd();
            assert!(fd > 2);
            in_child(move || {
                mark_descriptors()?;
                // SAFETY: fd is inherited from the parent.
                let flags = checked(unsafe { libc::fcntl(fd, libc::F_GETFD) })?;
                if flags & libc::FD_CLOEXEC == 0 {
                    return Err(io::Error::from_raw_os_error(libc::EINVAL));
                }
                Ok(())
            })
            .unwrap();
        }

        fn loopback_and_unreachable_peer() -> io::Result<()> {
            // SAFETY: all socket addresses and output buffers are stack-owned.
            unsafe {
                let listener = checked(libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0))?;
                let mut address = libc::sockaddr_in {
                    sin_family: libc::sa_family_t::try_from(libc::AF_INET).unwrap(),
                    sin_port: 0,
                    sin_addr: libc::in_addr {
                        s_addr: u32::from_ne_bytes([127, 0, 0, 1]),
                    },
                    sin_zero: [0; 8],
                };
                let mut len = libc::socklen_t::try_from(size_of::<libc::sockaddr_in>()).unwrap();
                checked(libc::bind(listener, (&raw const address).cast(), len))?;
                checked(libc::listen(listener, 1))?;
                checked(libc::getsockname(
                    listener,
                    (&raw mut address).cast(),
                    &raw mut len,
                ))?;
                let client = checked(libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0))?;
                checked(libc::connect(client, (&raw const address).cast(), len))?;
                let accepted = checked(libc::accept(
                    listener,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ))?;
                libc::close(accepted);
                libc::close(client);
                libc::close(listener);
                let remote = checked(libc::socket(
                    libc::AF_INET,
                    libc::SOCK_STREAM | libc::SOCK_NONBLOCK,
                    0,
                ))?;
                address.sin_addr.s_addr = u32::from_ne_bytes([192, 0, 2, 1]);
                address.sin_port = 80_u16.to_be();
                let result = libc::connect(remote, (&raw const address).cast(), len);
                let errno = io::Error::last_os_error().raw_os_error();
                libc::close(remote);
                if result != -1 || errno != Some(libc::ENETUNREACH) {
                    return Err(io::Error::from_raw_os_error(libc::EINVAL));
                }
                Ok(())
            }
        }

        #[test]
        fn an_offline_namespace_has_loopback_and_no_route_out() {
            let user_namespace = || {
                // SAFETY: unshare affects only this child.
                checked(unsafe { libc::unshare(libc::CLONE_NEWUSER) }).map(|_| ())
            };
            if let Err(error) = in_child(move || {
                user_namespace()?;
                // SAFETY: unshare affects only this child.
                checked(unsafe { libc::unshare(libc::CLONE_NEWNET) }).map(|_| ())
            }) {
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EPERM | libc::EINVAL | libc::ENOSPC | libc::EUSERS)
                ) {
                    eprintln!(
                        "skipping network isolation: an unprivileged user namespace is refused or \
                         has no capabilities (kernel.apparmor_restrict_unprivileged_userns?): {error}"
                    );
                    return;
                }
                panic!("create user and network namespaces: {error}");
            }
            in_child(move || {
                user_namespace()?;
                isolate_network()?;
                loopback_and_unreachable_peer()
            })
            .unwrap();
        }

        #[test]
        fn the_whole_jail_drops_identity_and_installs_the_deny_set() {
            use crate::process::jail::{BASE_UID, JailLimits};
            // SAFETY: geteuid has no preconditions.
            if unsafe { libc::geteuid() } != 0 {
                eprintln!("skipping whole guest jail: it requires euid 0");
                return;
            }
            let mapped = std::fs::read_to_string("/proc/self/uid_map")
                .unwrap()
                .lines()
                .any(|line| {
                    let fields = line
                        .split_whitespace()
                        .map(|n| n.parse::<u64>().unwrap())
                        .collect::<Vec<_>>();
                    fields[0] <= u64::from(BASE_UID) && u64::from(BASE_UID) < fields[0] + fields[2]
                });
            if !mapped {
                eprintln!(
                    "skipping whole guest jail: BASE_UID {BASE_UID} is outside /proc/self/uid_map"
                );
                return;
            }
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("cgroup.procs"), b"").unwrap();
            let plan = JailPlan {
                seq: Seq::new(0).unwrap(),
                net: None,
                cgroup: root.path().to_owned(),
                limits: JailLimits::default(),
            };
            let mut launch = crate::process::Launch::new("/bin/true");
            launch.apply_guest_jail(&plan).unwrap();
            // SAFETY: the probe only makes raw syscalls after the jail's hook.
            unsafe {
                launch.cmd.pre_exec(move || {
                    if libc::getuid() != plan.seq.uid() {
                        return Err(io::Error::from_raw_os_error(libc::EINVAL));
                    }
                    let vsock = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
                    let errno = io::Error::last_os_error().raw_os_error();
                    if vsock != -1 || errno != Some(libc::EPERM) {
                        return Err(io::Error::from_raw_os_error(libc::EINVAL));
                    }
                    let inet = checked(libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0))?;
                    libc::close(inet);
                    Ok(())
                });
            }
            assert!(launch.cmd.spawn().unwrap().wait().unwrap().success());
        }
    }
}
