//! The seccomp deny-set the payload runs under inside the bwrap envelope, on
//! x86-64 and aarch64: a value that renders to BPF for bwrap, to text for the
//! profile dump, and *names* a denied syscall number for `diag` — the same
//! data enforces and explains, so the two cannot drift.
//!
//! It is a deny-set, not an allow-list ([`design/two-enforcers`] in the wiki):
//! an envelope runs whatever `exec` admits, and an allow-list would turn every
//! unanticipated syscall into an unattributable SIGSYS. Every [`Rule`] carries
//! one of two verdicts — [`Verdict::Kill`] for kernel surface no unprivileged
//! program has a legitimate use for, [`Verdict::Errno`] for a call a
//! legitimate tool may attempt and should be told no in its own words — and a
//! `seccompiler` filter has one `match_action`, so the rules compile to one
//! BPF program per distinct verdict, stacked by bwrap (`--seccomp`, then
//! `--add-seccomp-fd` per further program): the kernel evaluates every
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
//! syscalls at all).

use std::fmt;

/// A syscall number paired with its name: the only place the two meet.
#[derive(Clone, Copy, Debug)]
struct Syscall {
    nr: i64,
    name: &'static str,
}

const PTRACE: Syscall = Syscall {
    nr: libc::SYS_ptrace,
    name: "ptrace",
};
const PROCESS_VM_READV: Syscall = Syscall {
    nr: libc::SYS_process_vm_readv,
    name: "process_vm_readv",
};
const PROCESS_VM_WRITEV: Syscall = Syscall {
    nr: libc::SYS_process_vm_writev,
    name: "process_vm_writev",
};
const PERF_EVENT_OPEN: Syscall = Syscall {
    nr: libc::SYS_perf_event_open,
    name: "perf_event_open",
};
const BPF: Syscall = Syscall {
    nr: libc::SYS_bpf,
    name: "bpf",
};
const KEXEC_LOAD: Syscall = Syscall {
    nr: libc::SYS_kexec_load,
    name: "kexec_load",
};
/// `libc` omits this one number from its musl/aarch64 bindings alone, so it is
/// spelled here rather than the rule going missing on a shipping target: 294
/// is the asm-generic number, which libc itself carries for glibc/aarch64 and
/// for musl/loongarch64.
#[cfg(all(target_arch = "aarch64", target_env = "musl"))]
const SYS_KEXEC_FILE_LOAD: i64 = 294;
#[cfg(not(all(target_arch = "aarch64", target_env = "musl")))]
const SYS_KEXEC_FILE_LOAD: i64 = libc::SYS_kexec_file_load;
const KEXEC_FILE_LOAD: Syscall = Syscall {
    nr: SYS_KEXEC_FILE_LOAD,
    name: "kexec_file_load",
};
const REBOOT: Syscall = Syscall {
    nr: libc::SYS_reboot,
    name: "reboot",
};
const SWAPON: Syscall = Syscall {
    nr: libc::SYS_swapon,
    name: "swapon",
};
const SWAPOFF: Syscall = Syscall {
    nr: libc::SYS_swapoff,
    name: "swapoff",
};
const INIT_MODULE: Syscall = Syscall {
    nr: libc::SYS_init_module,
    name: "init_module",
};
const FINIT_MODULE: Syscall = Syscall {
    nr: libc::SYS_finit_module,
    name: "finit_module",
};
const DELETE_MODULE: Syscall = Syscall {
    nr: libc::SYS_delete_module,
    name: "delete_module",
};
const KEYCTL: Syscall = Syscall {
    nr: libc::SYS_keyctl,
    name: "keyctl",
};
const ADD_KEY: Syscall = Syscall {
    nr: libc::SYS_add_key,
    name: "add_key",
};
const REQUEST_KEY: Syscall = Syscall {
    nr: libc::SYS_request_key,
    name: "request_key",
};
const USERFAULTFD: Syscall = Syscall {
    nr: libc::SYS_userfaultfd,
    name: "userfaultfd",
};
const OPEN_BY_HANDLE_AT: Syscall = Syscall {
    nr: libc::SYS_open_by_handle_at,
    name: "open_by_handle_at",
};
const MOUNT: Syscall = Syscall {
    nr: libc::SYS_mount,
    name: "mount",
};
const UMOUNT2: Syscall = Syscall {
    nr: libc::SYS_umount2,
    name: "umount2",
};
const PIVOT_ROOT: Syscall = Syscall {
    nr: libc::SYS_pivot_root,
    name: "pivot_root",
};
const MOVE_MOUNT: Syscall = Syscall {
    nr: libc::SYS_move_mount,
    name: "move_mount",
};
const OPEN_TREE: Syscall = Syscall {
    nr: libc::SYS_open_tree,
    name: "open_tree",
};
const FSOPEN: Syscall = Syscall {
    nr: libc::SYS_fsopen,
    name: "fsopen",
};
const FSMOUNT: Syscall = Syscall {
    nr: libc::SYS_fsmount,
    name: "fsmount",
};
const FSCONFIG: Syscall = Syscall {
    nr: libc::SYS_fsconfig,
    name: "fsconfig",
};
const FSPICK: Syscall = Syscall {
    nr: libc::SYS_fspick,
    name: "fspick",
};
const MOUNT_SETATTR: Syscall = Syscall {
    nr: libc::SYS_mount_setattr,
    name: "mount_setattr",
};
const UNSHARE: Syscall = Syscall {
    nr: libc::SYS_unshare,
    name: "unshare",
};
const CLONE: Syscall = Syscall {
    nr: libc::SYS_clone,
    name: "clone",
};
const SETNS: Syscall = Syscall {
    nr: libc::SYS_setns,
    name: "setns",
};
const CLONE3: Syscall = Syscall {
    nr: libc::SYS_clone3,
    name: "clone3",
};
const IOCTL: Syscall = Syscall {
    nr: libc::SYS_ioctl,
    name: "ioctl",
};

/// A kernel constant paired with its name, as [`Syscall`] pairs a number.
#[derive(Clone, Copy, Debug)]
struct Named {
    value: u32,
    name: &'static str,
}

#[allow(clippy::cast_sign_loss, reason = "a single positive flag bit")]
const CLONE_NEWUSER: Named = Named {
    value: libc::CLONE_NEWUSER as u32,
    name: "CLONE_NEWUSER",
};
/// `0x5412` on both x86-64 and aarch64.
#[allow(clippy::cast_possible_truncation, reason = "0x5412 fits a u32")]
const TIOCSTI: Named = Named {
    value: libc::TIOCSTI as u32,
    name: "TIOCSTI",
};

/// A condition on one raw syscall argument, compared as a `Dword`: every
/// value a rule below needs fits 32 bits, with the high half zero.
enum Cond {
    /// `arg & mask != 0`
    FlagSet { arg: u8, mask: Named },
    /// `arg == value`
    ArgEq { arg: u8, value: Named },
}

impl fmt::Display for Cond {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::FlagSet { arg, mask } => write!(f, "arg{arg} & {}", mask.name),
            Self::ArgEq { arg, value } => write!(f, "arg{arg} == {}", value.name),
        }
    }
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
/// `verdict` — and `why`, repeated verbatim by `Display` and by `diag`.
struct Rule {
    calls: &'static [Syscall],
    when: Option<Cond>,
    verdict: Verdict,
    why: &'static str,
}

/// The envelope's deny-set: a value, so it can be rendered, printed and
/// consulted without touching the kernel.
pub(crate) struct Filter(&'static [Rule]);

impl Filter {
    /// The deny-set. A `const`, not a `OnceLock`: nothing in it is probed.
    pub(crate) const ENVELOPE: Self = Self(&[
        Rule {
            calls: &[PTRACE, PROCESS_VM_READV, PROCESS_VM_WRITEV],
            when: None,
            verdict: Verdict::Kill,
            why: "reads or rewrites another process; the envelope hides the host's table, \
                  this closes the same-uid pair inside it",
        },
        Rule {
            calls: &[PERF_EVENT_OPEN, BPF],
            when: None,
            verdict: Verdict::Kill,
            why: "loads programs into the kernel; classic privilege-escalation surface",
        },
        Rule {
            calls: &[KEXEC_LOAD, KEXEC_FILE_LOAD, REBOOT, SWAPON, SWAPOFF],
            when: None,
            verdict: Verdict::Kill,
            why: "machine state",
        },
        Rule {
            calls: &[INIT_MODULE, FINIT_MODULE, DELETE_MODULE],
            when: None,
            verdict: Verdict::Kill,
            why: "kernel code",
        },
        Rule {
            calls: &[KEYCTL, ADD_KEY, REQUEST_KEY],
            when: None,
            verdict: Verdict::Kill,
            why: "the kernel keyring: cross-process state the envelope does not model",
        },
        Rule {
            calls: &[USERFAULTFD],
            when: None,
            verdict: Verdict::Kill,
            why: "the race-widening primitive in most modern exploit chains",
        },
        Rule {
            calls: &[OPEN_BY_HANDLE_AT],
            when: None,
            verdict: Verdict::Kill,
            why: "opens by inode handle, bypassing every path the envelope bound",
        },
        Rule {
            calls: &[
                MOUNT,
                UMOUNT2,
                PIVOT_ROOT,
                MOVE_MOUNT,
                OPEN_TREE,
                FSOPEN,
                FSMOUNT,
                FSCONFIG,
                FSPICK,
                MOUNT_SETATTR,
            ],
            when: None,
            verdict: Verdict::Kill,
            why: "mount authority inside the envelope hides or reshapes the deny masks",
        },
        Rule {
            calls: &[UNSHARE, CLONE],
            when: Some(Cond::FlagSet {
                arg: 0,
                mask: CLONE_NEWUSER,
            }),
            verdict: Verdict::Errno(libc::EPERM),
            why: "a user namespace is root over a fresh mount tree — a container inside the \
                  grant, which the envelope forbids",
        },
        Rule {
            calls: &[SETNS],
            when: None,
            verdict: Verdict::Errno(libc::EPERM),
            why: "joining another namespace is leaving this one",
        },
        Rule {
            calls: &[CLONE3],
            when: None,
            verdict: Verdict::Errno(libc::ENOSYS),
            why: "its flags live in a struct seccomp cannot read; ENOSYS makes glibc and Rust \
                  std fall back to `clone`, which the row above inspects (Docker's default \
                  profile does the same)",
        },
        Rule {
            calls: &[IOCTL],
            when: Some(Cond::ArgEq {
                arg: 1,
                value: TIOCSTI,
            }),
            verdict: Verdict::Errno(libc::EPERM),
            why: "terminal input injection (CVE-2017-5226); `--new-session` closes it, this is \
                  the seccomp half the namespace ADR deferred to the terminal trampoline, \
                  independent of it",
        },
    ]);

    /// What this filter says about syscall `nr`, for a `type=1326` record.
    /// `None` for a number the filter never denies — a record naming one
    /// came from another filter and is not ours to explain.
    pub(crate) fn explain(&self, nr: i64) -> Option<Denied<'_>> {
        self.0.iter().find_map(|rule| {
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

impl fmt::Display for Filter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for rule in self.0 {
            let calls = rule
                .calls
                .iter()
                .map(|call| call.name)
                .collect::<Vec<_>>()
                .join(", ");
            let verdict = match rule.verdict {
                Verdict::Kill => "SIGSYS",
                Verdict::Errno(code) => errno_name(code),
            };
            match &rule.when {
                Some(cond) => writeln!(f, "{calls} ({cond}) → {verdict}: {}", rule.why)?,
                None => writeln!(f, "{calls} → {verdict}: {}", rule.why)?,
            }
        }
        #[cfg(target_arch = "x86_64")]
        writeln!(f, "x32 ABI: killed outright (x86-64 only)")?;
        #[cfg(not(target_arch = "x86_64"))]
        writeln!(f, "no foreign ABI shares this arch's audit value")?;
        Ok(())
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
    /// fds `apply_seccomp` parks them at; the kernel evaluates every stacked
    /// filter independently and keeps the most severe result, so which fd is
    /// `--seccomp` and which are `--add-seccomp-fd` carries no meaning.
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
    fn compile(&self) -> Result<seccompiler::SeccompCondition, Error> {
        use seccompiler::{SeccompCmpArgLen::Dword, SeccompCmpOp, SeccompCondition};
        match *self {
            Self::FlagSet { arg, mask } => SeccompCondition::new(
                arg,
                Dword,
                SeccompCmpOp::MaskedEq(u64::from(mask.value)),
                u64::from(mask.value),
            ),
            Self::ArgEq { arg, value } => {
                SeccompCondition::new(arg, Dword, SeccompCmpOp::Eq, u64::from(value.value))
            }
        }
        .map_err(Error::Compile)
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
                    Some(cond) => {
                        vec![
                            seccompiler::SeccompRule::new(vec![cond.compile()?])
                                .map_err(Error::Compile)?,
                        ]
                    }
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

/// Print the deny-set for `RAL_DUMP_SANDBOX_PROFILE`.
pub(crate) fn dump() {
    eprintln!(
        "--- seccomp deny-set ---\n{}--- end seccomp deny-set ---",
        Filter::ENVELOPE
    );
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn explain_names_a_kill_an_errno_and_nothing_for_an_undenied_call() {
        let ptrace = Filter::ENVELOPE
            .explain(libc::SYS_ptrace)
            .expect("ptrace is denied");
        assert_eq!(ptrace.name, "ptrace");
        assert_eq!(ptrace.verdict, Verdict::Kill);

        let clone3 = Filter::ENVELOPE
            .explain(libc::SYS_clone3)
            .expect("clone3 is denied");
        assert_eq!(clone3.verdict, Verdict::Errno(libc::ENOSYS));

        assert!(Filter::ENVELOPE.explain(libc::SYS_read).is_none());
    }

    #[test]
    fn display_of_a_two_rule_filter_matches_a_literal() {
        const RULES: &[Rule] = &[
            Rule {
                calls: &[PTRACE],
                when: None,
                verdict: Verdict::Kill,
                why: "reads another process",
            },
            Rule {
                calls: &[UNSHARE, CLONE],
                when: Some(Cond::FlagSet {
                    arg: 0,
                    mask: CLONE_NEWUSER,
                }),
                verdict: Verdict::Errno(libc::EPERM),
                why: "a container inside the grant",
            },
        ];
        let filter = Filter(RULES);
        #[cfg(target_arch = "x86_64")]
        const ABI_LINE: &str = "x32 ABI: killed outright (x86-64 only)\n";
        #[cfg(not(target_arch = "x86_64"))]
        const ABI_LINE: &str = "no foreign ABI shares this arch's audit value\n";
        let expected = format!(
            "ptrace → SIGSYS: reads another process\n\
             unshare, clone (arg0 & CLONE_NEWUSER) → EPERM: a container inside the grant\n\
             {ABI_LINE}"
        );
        assert_eq!(filter.to_string(), expected);
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn programs_compiles_one_program_per_distinct_verdict() {
        let programs = Filter::ENVELOPE.programs().expect("the deny-set compiles");
        // Kill; Errno(EPERM) (unshare/clone, setns, ioctl); Errno(ENOSYS) (clone3).
        assert_eq!(programs.verdicts.len(), 3);
        for (_, bytes) in &programs.verdicts {
            assert!(!bytes.is_empty());
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
