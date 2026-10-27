use super::*;

fn probe_errno(result: io::Result<()>) -> Option<i32> {
    result.err().map(|error| {
        error
            .raw_os_error()
            .expect("a syscall probe returns an OS error")
    })
}

fn compare_probe(
    name: &str,
    refused: bool,
    probe: impl Fn() -> io::Result<()> + Copy + Send + Sync + 'static,
) {
    let deny = Filter::ENVELOPE.programs().expect("the deny-set compiles");
    let unfiltered = probe_errno(in_child(probe));
    let filtered = probe_errno(in_child(move || {
        deny.install()?;
        probe()
    }));
    if refused && unfiltered == Some(libc::EPERM) {
        eprintln!(
            "skipping {name}: the unfiltered call returns EPERM; host policy, missing \
             CAP_NET_RAW or kernel.io_uring_disabled may already refuse it"
        );
        return;
    }
    if refused {
        assert_eq!(filtered, Some(libc::EPERM), "{name}: filtered errno");
    } else {
        assert_eq!(filtered, unfiltered, "{name}: the deny-set must admit it");
    }
}

fn socket(family: u64, kind: i32, protocol: i32) -> io::Result<()> {
    // SAFETY: socket takes integer arguments; its result belongs to this child.
    let fd = unsafe { libc::syscall(libc::SYS_socket, family, kind, protocol) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd was just opened by this probe.
    unsafe { libc::syscall(libc::SYS_close, fd) };
    Ok(())
}

fn socketpair(family: i32) -> io::Result<()> {
    let mut fds = [-1; 2];
    // SAFETY: fds has space for both descriptors.
    let result = unsafe { libc::socketpair(family, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    for fd in fds {
        // SAFETY: both descriptors were just opened by this probe.
        unsafe { libc::close(fd) };
    }
    Ok(())
}

fn io_uring(nr: i64, first: i64) -> io::Result<()> {
    // SAFETY: null buffers and fd -1 cannot access memory or an existing ring.
    let result = unsafe { libc::syscall(nr, first, 0_i64, 0_i64, 0_i64, 0_i64, 0_i64) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[test]
fn socket_families_outside_the_grant_model_are_refused() {
    for (name, family, kind, protocol) in [
        ("AF_VSOCK", libc::AF_VSOCK, libc::SOCK_STREAM, 0),
        ("AF_PACKET", libc::AF_PACKET, libc::SOCK_RAW, 0),
        ("AF_ALG", libc::AF_ALG, libc::SOCK_SEQPACKET, 0),
        (
            "NETLINK_SOCK_DIAG",
            libc::AF_NETLINK,
            libc::SOCK_RAW,
            libc::NETLINK_SOCK_DIAG,
        ),
        (
            "NETLINK_KOBJECT_UEVENT",
            libc::AF_NETLINK,
            libc::SOCK_DGRAM,
            libc::NETLINK_KOBJECT_UEVENT,
        ),
    ] {
        compare_probe(name, true, move || {
            socket(u64::from(family.cast_unsigned()), kind, protocol)
        });
    }
    compare_probe("AF_TIPC socketpair", true, || socketpair(libc::AF_TIPC));
    compare_probe("AF_VSOCK with high bits", true, || {
        socket(
            (1_u64 << 32) | u64::from(libc::AF_VSOCK.cast_unsigned()),
            libc::SOCK_STREAM,
            0,
        )
    });
}

#[test]
fn internet_routing_and_unix_sockets_remain_admitted() {
    for (name, family, kind, protocol) in [
        ("AF_INET stream", libc::AF_INET, libc::SOCK_STREAM, 0),
        ("AF_INET datagram", libc::AF_INET, libc::SOCK_DGRAM, 0),
        ("AF_INET6 stream", libc::AF_INET6, libc::SOCK_STREAM, 0),
        ("AF_INET6 datagram", libc::AF_INET6, libc::SOCK_DGRAM, 0),
        (
            "NETLINK_ROUTE",
            libc::AF_NETLINK,
            libc::SOCK_RAW,
            libc::NETLINK_ROUTE,
        ),
        ("AF_UNIX", libc::AF_UNIX, libc::SOCK_STREAM, 0),
    ] {
        compare_probe(name, false, move || {
            socket(u64::from(family.cast_unsigned()), kind, protocol)
        });
    }
    compare_probe("AF_UNIX socketpair", false, || socketpair(libc::AF_UNIX));
    compare_probe("AF_INET with high bits", false, || {
        socket(
            (1_u64 << 32) | u64::from(libc::AF_INET.cast_unsigned()),
            libc::SOCK_STREAM,
            0,
        )
    });
}

#[test]
fn io_uring_cannot_bypass_the_socket_rules() {
    for (name, nr, first) in [
        ("io_uring_setup", libc::SYS_io_uring_setup, 0),
        ("io_uring_enter", libc::SYS_io_uring_enter, -1),
        ("io_uring_register", libc::SYS_io_uring_register, -1),
    ] {
        compare_probe(name, true, move || io_uring(nr, first));
    }
}

#[cfg(seccomp)]
fn sacrificial(role: &'static str, name: &str) -> std::process::Output {
    let thread = std::thread::current();
    let test = thread.name().expect("libtest names its test thread");
    std::process::Command::new(std::env::current_exe().expect("the test binary's path"))
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(role, name)
        .output()
        .expect("start a sacrificial seccomp child")
}

#[cfg(seccomp)]
#[test]
fn terminal_injection_is_refused_by_installed_programs() {
    use crate::test_helper::confinement::Output;
    use std::os::fd::AsRawFd;

    const ROLE: &str = "RAL_TEST_TERMINAL_INJECTION";
    const MARKER: &str = "terminal-injection-probe-returned";

    if std::env::var_os(ROLE).is_some() {
        Filter::ENVELOPE
            .programs()
            .expect("the deny-set compiles")
            .install()
            .expect("install the deny-set");
        // libtest's `test … ` prefix holds the current line.
        println!();
        let file = std::fs::File::open("/dev/null").expect("open the ioctl control");
        for (name, request) in [
            ("TIOCGWINSZ", libc::TIOCGWINSZ),
            ("TIOCSTI", libc::TIOCSTI),
            ("TIOCLINUX", libc::TIOCLINUX),
        ] {
            // SAFETY: winsize is plain integers; the pointer covers the control's output.
            let mut size: libc::winsize = unsafe { std::mem::zeroed() };
            // SAFETY: file is open and size is writable for the ioctl control.
            assert_eq!(
                unsafe { libc::ioctl(file.as_raw_fd(), request, &raw mut size) },
                -1
            );
            let errno = std::io::Error::last_os_error()
                .raw_os_error()
                .expect("ioctl returned an OS error");
            println!("{name} {errno}");
        }
        println!("{MARKER}");
        return;
    }

    let out: Output = sacrificial(ROLE, "probe").into();
    assert_eq!(out.status, 0, "{out:?}");
    assert!(out.stdout.contains(MARKER), "{out:?}");
    for (name, errno) in [
        ("TIOCGWINSZ", libc::ENOTTY),
        ("TIOCSTI", libc::EPERM),
        ("TIOCLINUX", libc::EPERM),
    ] {
        assert!(
            out.stdout
                .lines()
                .any(|line| line == format!("{name} {errno}")),
            "{out:?}"
        );
    }
}

#[cfg(seccomp)]
#[test]
fn process_reach_is_killed_by_installed_programs() {
    use crate::test_helper::confinement::Output;
    use std::io::Write;

    const ROLE: &str = "RAL_TEST_PROCESS_REACH";
    const MARKER: &str = "process-reach-probe-returned";
    const CALLS: &[Syscall] = &[
        syscall!(SYS_pidfd_getfd),
        syscall!(SYS_process_madvise),
        syscall!(SYS_process_mrelease),
        syscall!(SYS_kcmp),
        #[cfg(target_arch = "x86_64")]
        syscall!(SYS_modify_ldt),
    ];

    if let Ok(name) = std::env::var(ROLE) {
        let deny = Filter::ENVELOPE.programs().expect("the deny-set compiles");
        // SAFETY: only the child installs filters; the parent reaps it.
        let child = unsafe { libc::fork() };
        assert!(
            child >= 0,
            "fork seccomp probe: {}",
            std::io::Error::last_os_error()
        );
        if child > 0 {
            let mut status = 0;
            loop {
                // SAFETY: child is ours and status is writable.
                let waited = unsafe { libc::waitpid(child, &raw mut status, 0) };
                if waited == child {
                    break;
                }
                let error = std::io::Error::last_os_error();
                assert_eq!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted,
                    "wait for seccomp probe: {error}"
                );
            }
            if libc::WIFSIGNALED(status) {
                println!("probe-signal {}", libc::WTERMSIG(status));
            } else {
                assert!(libc::WIFEXITED(status), "unexpected wait status: {status}");
                assert_eq!(libc::WEXITSTATUS(status), 0, "seccomp probe failed");
            }
            return;
        }
        deny.install().expect("install the deny-set");
        // SAFETY: getpid takes no arguments.
        let pid = unsafe { libc::syscall(libc::SYS_getpid) };
        assert_eq!(pid, i64::from(std::process::id()));
        println!("ordinary-syscall-succeeded");
        std::io::stdout()
            .flush()
            .expect("flush the default-action control");
        if name != "control" {
            let call = CALLS
                .iter()
                .find(|call| call.name == name)
                .expect("a named probe");
            // SAFETY: zero arguments provide no memory or process authority.
            unsafe { libc::syscall(call.nr, 0_i64, 0_i64, 0_i64, 0_i64, 0_i64, 0_i64) };
        }
        println!("{MARKER}");
        std::io::stdout().flush().expect("flush the probe marker");
        // SAFETY: the fork child must exit without returning to libtest.
        unsafe { libc::_exit(0) };
    }

    let control: Output = sacrificial(ROLE, "control").into();
    assert_eq!(control.status, 0, "{control:?}");
    assert!(control.stdout.contains(MARKER), "{control:?}");
    assert!(
        control.stdout.contains("ordinary-syscall-succeeded"),
        "{control:?}"
    );
    for call in CALLS {
        let out: Output = sacrificial(ROLE, call.name).into();
        assert_eq!(out.status, 0, "{}: {out:?}", call.name);
        assert!(
            out.stdout
                .lines()
                .any(|line| line == format!("probe-signal {}", libc::SIGSYS)),
            "{}: {out:?}",
            call.name
        );
        assert!(
            out.stdout.contains("ordinary-syscall-succeeded"),
            "{}: {out:?}",
            call.name
        );
        assert!(!out.stdout.contains(MARKER), "{}: {out:?}", call.name);
    }
}

#[test]
fn process_signals_remain_outside_the_deny_set() {
    for nr in [
        libc::SYS_kill,
        libc::SYS_pidfd_open,
        libc::SYS_pidfd_send_signal,
    ] {
        assert!(
            Filter::ENVELOPE
                .0
                .iter()
                .flat_map(|rule| rule.calls)
                .all(|call| call.nr != nr)
        );
    }
}

#[test]
fn every_why_is_non_empty_and_carries_no_trailing_full_stop() {
    for rule in Filter::ENVELOPE.0 {
        assert!(!rule.why.is_empty(), "a rule's why must not be empty");
        assert!(
            !rule.why.ends_with('.'),
            "the hint adds punctuation; why must not: {:?}",
            rule.why
        );
    }
}

#[test]
fn explain_names_a_kill_an_errno_and_nothing_for_an_undenied_call() {
    let ptrace = Filter::ENVELOPE
        .explain(libc::SYS_ptrace, &seccompiler::SeccompAction::KillThread)
        .expect("ptrace is denied");
    assert_eq!(ptrace.name, "ptrace");
    assert_eq!(ptrace.verdict, Verdict::Kill);

    let clone3 = Filter::ENVELOPE
        .explain(
            libc::SYS_clone3,
            &seccompiler::SeccompAction::Errno(libc::ENOSYS.unsigned_abs()),
        )
        .expect("clone3 is denied");
    assert_eq!(clone3.verdict, Verdict::Errno(libc::ENOSYS));

    let ioctl = Filter::ENVELOPE
        .explain(
            libc::SYS_ioctl,
            &seccompiler::SeccompAction::Errno(libc::EPERM.unsigned_abs()),
        )
        .expect("terminal injection is denied");
    assert_eq!(ioctl.name, "ioctl");
    assert_eq!(ioctl.verdict, Verdict::Errno(libc::EPERM));

    assert!(
        Filter::ENVELOPE
            .explain(libc::SYS_read, &seccompiler::SeccompAction::KillThread)
            .is_none()
    );
    for action in [
        seccompiler::SeccompAction::Errno(libc::EPERM.unsigned_abs()),
        seccompiler::SeccompAction::KillProcess,
        seccompiler::SeccompAction::Trap,
    ] {
        assert!(
            Filter::ENVELOPE
                .explain(libc::SYS_ptrace, &action)
                .is_none()
        );
    }
    assert!(
        Filter::ENVELOPE
            .explain(
                libc::SYS_clone3,
                &seccompiler::SeccompAction::Errno(libc::EPERM.unsigned_abs())
            )
            .is_none()
    );
}

#[cfg(seccomp)]
#[test]
fn programs_compiles_one_program_per_distinct_verdict() {
    let programs = Filter::ENVELOPE.programs().expect("the deny-set compiles");
    // Kill, EPERM, ENOSYS.
    assert_eq!(programs.verdicts.len(), 3);
    for (_, program) in &programs.verdicts {
        assert_ne!(program.len(), 0);
        assert!(program.len() <= usize::try_from(libc::BPF_MAXINSNS).unwrap());
    }
    #[cfg(target_arch = "x86_64")]
    assert!(
        programs.abi_guard.is_some(),
        "x86-64 must carry the x32 guard"
    );
    #[cfg(target_arch = "aarch64")]
    assert!(
        programs.abi_guard.is_none(),
        "aarch64 shares its audit value with no other ABI"
    );
}

#[cfg(target_arch = "x86_64")]
#[test]
fn the_x32_guard_kills_on_arch_mismatch_and_on_the_high_syscall_bit() {
    use seccompiler::sock_filter;
    let expected = [
        sock_filter {
            code: 0x20,
            jt: 0,
            jf: 0,
            k: 4,
        },
        sock_filter {
            code: 0x15,
            jt: 1,
            jf: 0,
            k: 0xC000_003E,
        },
        sock_filter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_KILL_THREAD,
        },
        sock_filter {
            code: 0x20,
            jt: 0,
            jf: 0,
            k: 0,
        },
        sock_filter {
            code: 0x35,
            jt: 0,
            jf: 1,
            k: 0x4000_0000,
        },
        sock_filter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_KILL_THREAD,
        },
        sock_filter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ALLOW,
        },
    ];
    assert_eq!(x32_guard_program(), expected);
}
