//! The seccomp deny-set the payload runs under inside the bwrap envelope, on
//! x86-64 and aarch64: a value that renders to BPF for bwrap and *names* a
//! denied syscall number for `diag` — the same data enforces and explains, so
//! the two cannot drift.
//!
//! It is a deny-set, not an allow-list ([`design/two-enforcers`] in the wiki):
//! an envelope runs whatever `exec` admits, and an allow-list would turn every
//! unanticipated syscall into an unattributable SIGSYS. Every [`Rule`] carries
//! one of two verdicts — [`Verdict::Kill`] for kernel surface no unprivileged
//! program has a legitimate use for, [`Verdict::Errno`] for a call a
//! legitimate tool may attempt and should be told no in its own words — and a
//! `seccompiler` filter has one `match_action`, so the rules compile to one
//! BPF program per distinct verdict, stacked by bwrap (one
//! `--add-seccomp-fd` per program): the kernel evaluates every
//! installed filter independently and takes the most severe result, so
//! program order carries no meaning.
//!
//! The filter is projection-independent — an envelope invariant keyed on
//! nothing in the grant — so it is compiled once per process, not per launch.
//!
//! **The x32 guard.** `seccompiler`'s arch-validation prologue
//! (`backend/bpf.rs::build_arch_validation_sequence`) only compares
//! `seccomp_data.arch` against the running ABI's `AUDIT_ARCH`; it never reads
//! bit 30 of `nr`. x32 shares `AUDIT_ARCH_X86_64`, so an x32 syscall passes
//! that prologue, matches no key in our syscall-numbered maps, and reaches
//! the kill program's own mismatch action — `Allow`. So the guard stays
//! ours: one more stacked program, `x86_64` only, a complete filter in its own
//! right (the kernel evaluates each stacked program independently, so a
//! guard riding on another program's arch check would not see foreign-ABI
//! syscalls at all). ia32 and `AArch32` entries carry a foreign `AUDIT_ARCH`,
//! which seccompiler's arch prologue kills with `SECCOMP_RET_KILL_PROCESS`,
//! so x32 alone needs the guard.

use std::fmt;

/// A syscall number paired with its name: the only place the two meet.
#[derive(Clone, Copy, Debug)]
struct Syscall {
    nr: i64,
    name: &'static str,
}

impl Syscall {
    const fn new(nr: i64, name: &'static str) -> Self {
        Self { nr, name }
    }
}

/// `libc` omits this one number from its musl/aarch64 bindings alone, so it is
/// spelled here rather than the rule going missing on a shipping target: 294
/// is the asm-generic number, which libc itself carries for glibc/aarch64 and
/// for musl/loongarch64.
#[cfg(all(target_arch = "aarch64", target_env = "musl"))]
const SYS_KEXEC_FILE_LOAD: i64 = 294;
#[cfg(not(all(target_arch = "aarch64", target_env = "musl")))]
const SYS_KEXEC_FILE_LOAD: i64 = libc::SYS_kexec_file_load;

#[allow(clippy::cast_sign_loss, reason = "a single positive flag bit")]
const CLONE_NEWUSER: u32 = libc::CLONE_NEWUSER as u32;
/// `0x5412` and `0x541C` on both x86-64 and aarch64.
#[allow(clippy::cast_possible_truncation, reason = "0x5412 fits a u32")]
const TIOCSTI: u32 = libc::TIOCSTI as u32;
#[allow(clippy::cast_possible_truncation, reason = "0x541C fits a u32")]
const TIOCLINUX: u32 = libc::TIOCLINUX as u32;

/// A condition on one raw syscall argument, compared as a `Dword`: every
/// value a rule below needs fits 32 bits, with the high half zero.
enum Cond {
    /// `arg & mask != 0`
    FlagSet { arg: u8, mask: u32 },
    /// `arg` in `values`; non-empty, since seccompiler reads an empty chain as unconditional.
    ArgIn { arg: u8, values: &'static [u32] },
}

/// What a rule does to the syscalls it names. `Errno` holds a `libc::E*`
/// value; render casts it to the `u32` seccomp wants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Verdict {
    Kill,
    Errno(i32),
}

/// `EPERM`/`ENOSYS` by name — the only two this deny-set uses.
fn errno_name(code: i32) -> &'static str {
    match code {
        libc::EPERM => "EPERM",
        libc::ENOSYS => "ENOSYS",
        _ => "an unnamed errno",
    }
}

/// One deny-set entry: every syscall in `calls`, under `when` if present, gets
/// `verdict` — and `why`, repeated verbatim by `diag`.
struct Rule {
    calls: &'static [Syscall],
    when: Option<Cond>,
    verdict: Verdict,
    why: &'static str,
}

/// The envelope's deny-set: a value, so it can be rendered and
/// consulted without touching the kernel.
pub(crate) struct Filter(&'static [Rule]);

impl Filter {
    /// The deny-set. A `const`, not a `OnceLock`: nothing in it is probed.
    pub(crate) const ENVELOPE: Self = Self(&[
        Rule {
            calls: &[
                Syscall::new(libc::SYS_ptrace, "ptrace"),
                Syscall::new(libc::SYS_process_vm_readv, "process_vm_readv"),
                Syscall::new(libc::SYS_process_vm_writev, "process_vm_writev"),
                Syscall::new(libc::SYS_pidfd_getfd, "pidfd_getfd"),
                Syscall::new(libc::SYS_process_madvise, "process_madvise"),
                Syscall::new(libc::SYS_process_mrelease, "process_mrelease"),
                Syscall::new(libc::SYS_kcmp, "kcmp"),
            ],
            when: None,
            verdict: Verdict::Kill,
            why: "reads or rewrites another process; the envelope hides the host's table, \
                  this closes the same-uid pair inside it",
        },
        Rule {
            calls: &[
                Syscall::new(libc::SYS_perf_event_open, "perf_event_open"),
                Syscall::new(libc::SYS_bpf, "bpf"),
                #[cfg(target_arch = "x86_64")]
                Syscall::new(libc::SYS_modify_ldt, "modify_ldt"),
            ],
            when: None,
            verdict: Verdict::Kill,
            why: "loads programs into the kernel; classic privilege-escalation surface",
        },
        Rule {
            calls: &[
                Syscall::new(libc::SYS_kexec_load, "kexec_load"),
                Syscall::new(SYS_KEXEC_FILE_LOAD, "kexec_file_load"),
                Syscall::new(libc::SYS_reboot, "reboot"),
                Syscall::new(libc::SYS_swapon, "swapon"),
                Syscall::new(libc::SYS_swapoff, "swapoff"),
            ],
            when: None,
            verdict: Verdict::Kill,
            why: "machine state",
        },
        Rule {
            calls: &[
                Syscall::new(libc::SYS_init_module, "init_module"),
                Syscall::new(libc::SYS_finit_module, "finit_module"),
                Syscall::new(libc::SYS_delete_module, "delete_module"),
            ],
            when: None,
            verdict: Verdict::Kill,
            why: "kernel code",
        },
        Rule {
            calls: &[
                Syscall::new(libc::SYS_keyctl, "keyctl"),
                Syscall::new(libc::SYS_add_key, "add_key"),
                Syscall::new(libc::SYS_request_key, "request_key"),
            ],
            when: None,
            verdict: Verdict::Kill,
            why: "the kernel keyring: cross-process state the envelope does not model",
        },
        Rule {
            calls: &[Syscall::new(libc::SYS_userfaultfd, "userfaultfd")],
            when: None,
            verdict: Verdict::Kill,
            why: "the race-widening primitive in most modern exploit chains",
        },
        Rule {
            calls: &[Syscall::new(
                libc::SYS_open_by_handle_at,
                "open_by_handle_at",
            )],
            when: None,
            verdict: Verdict::Kill,
            why: "opens by inode handle, bypassing every path the envelope bound",
        },
        Rule {
            calls: &[
                Syscall::new(libc::SYS_mount, "mount"),
                Syscall::new(libc::SYS_umount2, "umount2"),
                Syscall::new(libc::SYS_pivot_root, "pivot_root"),
                Syscall::new(libc::SYS_move_mount, "move_mount"),
                Syscall::new(libc::SYS_open_tree, "open_tree"),
                Syscall::new(libc::SYS_fsopen, "fsopen"),
                Syscall::new(libc::SYS_fsmount, "fsmount"),
                Syscall::new(libc::SYS_fsconfig, "fsconfig"),
                Syscall::new(libc::SYS_fspick, "fspick"),
                Syscall::new(libc::SYS_mount_setattr, "mount_setattr"),
            ],
            when: None,
            verdict: Verdict::Kill,
            why: "mount authority inside the envelope hides or reshapes the deny masks",
        },
        Rule {
            calls: &[
                Syscall::new(libc::SYS_unshare, "unshare"),
                Syscall::new(libc::SYS_clone, "clone"),
            ],
            when: Some(Cond::FlagSet {
                arg: 0,
                mask: CLONE_NEWUSER,
            }),
            verdict: Verdict::Errno(libc::EPERM),
            why: "a user namespace is root over a fresh mount tree: a container inside the \
                  grant, which the envelope forbids",
        },
        Rule {
            calls: &[Syscall::new(libc::SYS_setns, "setns")],
            when: None,
            verdict: Verdict::Errno(libc::EPERM),
            why: "joining another namespace is leaving this one",
        },
        Rule {
            calls: &[Syscall::new(libc::SYS_clone3, "clone3")],
            when: None,
            verdict: Verdict::Errno(libc::ENOSYS),
            why: "its flags live in a struct seccomp cannot read; ENOSYS makes glibc and Rust \
                  std fall back to `clone`, which the row above inspects (Docker's default \
                  profile does the same)",
        },
        Rule {
            calls: &[Syscall::new(libc::SYS_ioctl, "ioctl")],
            when: Some(Cond::ArgIn {
                arg: 1,
                values: &[TIOCSTI, TIOCLINUX],
            }),
            verdict: Verdict::Errno(libc::EPERM),
            why: "terminal input injection (CVE-2017-5226); `--new-session` closes it, this is \
                  the seccomp half the namespace ADR deferred to the terminal trampoline, \
                  independent of it",
        },
    ]);

    /// What this filter says about syscall `nr`, for a `type=1326` record.
    /// Only a matching syscall and action can belong to this filter.
    pub(crate) fn explain(
        &self,
        nr: i64,
        action: &seccompiler::SeccompAction,
    ) -> Option<Denied<'_>> {
        self.0.iter().find_map(|rule| {
            if &rule.verdict.action() != action {
                return None;
            }
            rule.calls
                .iter()
                .find(|call| call.nr == nr)
                .map(|call| Denied {
                    name: call.name,
                    verdict: rule.verdict,
                    why: rule.why,
                })
        })
    }
}

/// What the filter says about one denied syscall: its name, the verdict, and
/// the rule's own reason, repeated verbatim by `diag`.
pub(crate) struct Denied<'a> {
    pub(crate) name: &'static str,
    pub(crate) verdict: Verdict,
    why: &'a str,
}

impl fmt::Display for Denied<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.verdict {
            Verdict::Kill => write!(f, "`{}` (killed with SIGSYS): {}", self.name, self.why),
            Verdict::Errno(code) => {
                write!(
                    f,
                    "`{}` (refused with {}): {}",
                    self.name,
                    errno_name(code),
                    self.why
                )
            }
        }
    }
}

/// The two (or three, on x86-64) BPF programs bwrap reads: one per distinct
/// [`Verdict`] in the deny-set, plus the x32 guard on x86-64.
pub(crate) struct Programs {
    /// One compiled program per distinct verdict, in order of first
    /// appearance in [`Filter::ENVELOPE`] — `Kill`, then each distinct
    /// `Errno`.
    verdicts: Vec<(Verdict, Vec<u8>)>,
    /// The x32 guard (module doc): `Some` on x86-64 only.
    abi_guard: Option<Vec<u8>>,
}

impl Programs {
    /// Every compiled program, in a fixed order — this order only numbers the
    /// slots the envelope inherits them at; the kernel evaluates every stacked
    /// filter independently and keeps the most severe result, so which
    /// program lands on which fd carries no meaning.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &[u8]> {
        self.verdicts
            .iter()
            .map(|(_, bytes)| bytes.as_slice())
            .chain(self.abi_guard.as_deref())
    }
}

/// Which stage refused to compile the deny-set.
#[derive(Debug)]
pub(crate) enum Error {
    /// `std::env::consts::ARCH` is neither x86-64 nor aarch64; cannot happen
    /// under this file's arch cfg, but this is a `Result`, not a panic.
    Arch(seccompiler::BackendError),
    Compile(seccompiler::BackendError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arch(e) => write!(f, "seccomp: {e}"),
            Self::Compile(e) => write!(f, "seccomp: the deny-set failed to compile: {e}"),
        }
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
impl Cond {
    fn compile(&self) -> Result<Vec<seccompiler::SeccompRule>, Error> {
        use seccompiler::{SeccompCmpArgLen::Dword, SeccompCmpOp, SeccompCondition, SeccompRule};
        match *self {
            Self::FlagSet { arg, mask } => Ok(vec![
                SeccompRule::new(vec![
                    SeccompCondition::new(
                        arg,
                        Dword,
                        SeccompCmpOp::MaskedEq(u64::from(mask)),
                        u64::from(mask),
                    )
                    .map_err(Error::Compile)?,
                ])
                .map_err(Error::Compile)?,
            ]),
            Self::ArgIn { arg, values } => values
                .iter()
                .map(|&value| {
                    SeccompRule::new(vec![SeccompCondition::new(
                        arg,
                        Dword,
                        SeccompCmpOp::Eq,
                        u64::from(value),
                    )?])
                })
                .collect::<Result<_, _>>()
                .map_err(Error::Compile),
        }
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
impl Verdict {
    fn action(self) -> seccompiler::SeccompAction {
        match self {
            Self::Kill => seccompiler::SeccompAction::KillThread,
            #[allow(
                clippy::cast_sign_loss,
                reason = "libc errno constants are small positive values"
            )]
            Self::Errno(code) => seccompiler::SeccompAction::Errno(code as u32),
        }
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
impl Filter {
    /// The compiled programs, as the raw `sock_filter` bytes bwrap reads.
    /// Compiled once per process: a filter that will not compile is cached
    /// as a failure only implicitly — a failing compile is a static bug in
    /// [`Filter::ENVELOPE`], so recomputing it on every call costs nothing in
    /// practice and keeps this fallible without a `Clone` bound on `Error`.
    pub(crate) fn programs(&self) -> Result<&'static Programs, Error> {
        static PROGRAMS: std::sync::OnceLock<Programs> = std::sync::OnceLock::new();
        if let Some(programs) = PROGRAMS.get() {
            return Ok(programs);
        }
        let compiled = self.compile()?;
        Ok(PROGRAMS.get_or_init(|| compiled))
    }

    fn compile(&self) -> Result<Programs, Error> {
        let arch: seccompiler::TargetArch =
            std::env::consts::ARCH.try_into().map_err(Error::Arch)?;

        // Grouped by verdict, in order of first appearance, each carrying the
        // per-syscall rule map a `SeccompFilter` wants.
        let mut grouped: Vec<(
            Verdict,
            std::collections::BTreeMap<i64, Vec<seccompiler::SeccompRule>>,
        )> = Vec::new();
        for rule in self.0 {
            if !grouped.iter().any(|(verdict, _)| *verdict == rule.verdict) {
                grouped.push((rule.verdict, std::collections::BTreeMap::new()));
            }
            let (_, map) = grouped
                .iter_mut()
                .find(|(verdict, _)| *verdict == rule.verdict)
                .expect("just inserted above");
            for call in rule.calls {
                let chain = match &rule.when {
                    None => Vec::new(),
                    Some(cond) => cond.compile()?,
                };
                let prior = map.insert(call.nr, chain);
                debug_assert!(prior.is_none(), "{} named in two rules", call.name);
            }
        }

        let mut verdicts = Vec::with_capacity(grouped.len());
        for (verdict, rules) in grouped {
            let filter = seccompiler::SeccompFilter::new(
                rules,
                seccompiler::SeccompAction::Allow,
                verdict.action(),
                arch,
            )
            .map_err(Error::Compile)?;
            let bpf: seccompiler::BpfProgram = filter.try_into().map_err(Error::Compile)?;
            verdicts.push((verdict, serialise(&bpf)));
        }

        #[cfg(target_arch = "x86_64")]
        let abi_guard = Some(serialise(&x32_guard_program()));
        #[cfg(not(target_arch = "x86_64"))]
        let abi_guard = None;

        Ok(Programs {
            verdicts,
            abi_guard,
        })
    }
}

/// Serialise a compiled `sock_filter` program field-by-field, eight bytes per
/// instruction — the same `sock_fprog` layout the deleted `BpfProg` wrote by
/// hand; no `unsafe` cast.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn serialise(prog: &[seccompiler::sock_filter]) -> Vec<u8> {
    let mut out = Vec::with_capacity(prog.len() * 8);
    for insn in prog {
        out.extend_from_slice(&insn.code.to_le_bytes());
        out.push(insn.jt);
        out.push(insn.jf);
        out.extend_from_slice(&insn.k.to_le_bytes());
    }
    out
}

/// The x32 guard's seven instructions (module doc). The arch check is
/// repeated here on purpose: the kernel evaluates each stacked program
/// independently, so this program must stand alone.
#[cfg(target_arch = "x86_64")]
fn x32_guard_program() -> [seccompiler::sock_filter; 7] {
    use seccompiler::sock_filter;
    const LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
    const JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
    const JGE_K: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
    const RET_K: u16 = 0x06; // BPF_RET | BPF_K
    // Offsets into the kernel's seccomp_data struct.
    const NR_OFFSET: u32 = 0;
    const ARCH_OFFSET: u32 = 4;
    const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
    const X32_SYSCALL_BIT: u32 = 0x4000_0000;
    [
        sock_filter {
            code: LD_W_ABS,
            jt: 0,
            jf: 0,
            k: ARCH_OFFSET,
        },
        sock_filter {
            code: JEQ_K,
            jt: 1,
            jf: 0,
            k: AUDIT_ARCH_X86_64,
        },
        sock_filter {
            code: RET_K,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_KILL_THREAD,
        },
        sock_filter {
            code: LD_W_ABS,
            jt: 0,
            jf: 0,
            k: NR_OFFSET,
        },
        sock_filter {
            code: JGE_K,
            jt: 0,
            jf: 1,
            k: X32_SYSCALL_BIT,
        },
        sock_filter {
            code: RET_K,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_KILL_THREAD,
        },
        sock_filter {
            code: RET_K,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ALLOW,
        },
    ]
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
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
            Syscall::new(libc::SYS_pidfd_getfd, "pidfd_getfd"),
            Syscall::new(libc::SYS_process_madvise, "process_madvise"),
            Syscall::new(libc::SYS_process_mrelease, "process_mrelease"),
            Syscall::new(libc::SYS_kcmp, "kcmp"),
            #[cfg(target_arch = "x86_64")]
            Syscall::new(libc::SYS_modify_ldt, "modify_ldt"),
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
}
