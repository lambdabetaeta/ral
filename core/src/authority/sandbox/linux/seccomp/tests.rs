use super::*;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn install_envelope_programs() {
    for bytes in Filter::ENVELOPE
        .programs()
        .expect("the deny-set compiles")
        .iter()
    {
        let bpf: Vec<_> = bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|insn| seccompiler::sock_filter {
                code: u16::from_le_bytes([insn[0], insn[1]]),
                jt: insn[2],
                jf: insn[3],
                k: u32::from_le_bytes([insn[4], insn[5], insn[6], insn[7]]),
            })
            .collect();
        seccompiler::apply_filter(&bpf).expect("install an envelope program");
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn sacrificial(role: &'static str, name: &str) -> std::process::Output {
    let thread = std::thread::current();
    let test = thread.name().expect("libtest names its test thread");
    std::process::Command::new(std::env::current_exe().expect("the test binary's path"))
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(role, name)
        .output()
        .expect("start a sacrificial seccomp child")
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[test]
fn terminal_injection_is_refused_by_installed_programs() {
    use crate::test_helper::confinement::Output;
    use std::os::fd::AsRawFd;

    const ROLE: &str = "RAL_TEST_TERMINAL_INJECTION";
    const MARKER: &str = "terminal-injection-probe-returned";

    if std::env::var_os(ROLE).is_some() {
        install_envelope_programs();
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

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
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
        install_envelope_programs();
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
fn no_syscall_is_named_in_two_rules() {
    let mut seen = std::collections::HashSet::new();
    for rule in Filter::ENVELOPE.0 {
        for call in rule.calls {
            assert!(seen.insert(call.nr), "{} named twice", call.name);
        }
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
fn no_condition_is_empty() {
    for rule in Filter::ENVELOPE.0 {
        if let Some(Cond::ArgIn { values, .. }) = &rule.when {
            assert!(!values.is_empty(), "an empty chain denies every argument");
        }
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

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[test]
fn programs_compiles_one_program_per_distinct_verdict() {
    let programs = Filter::ENVELOPE.programs().expect("the deny-set compiles");
    // Kill; Errno(EPERM) (unshare/clone, setns, ioctl); Errno(ENOSYS) (clone3).
    assert_eq!(programs.verdicts.len(), 3);
    for (_, bytes) in &programs.verdicts {
        assert_ne!(bytes, &Vec::<u8>::new());
        assert_eq!(bytes.len() % 8, 0);
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
