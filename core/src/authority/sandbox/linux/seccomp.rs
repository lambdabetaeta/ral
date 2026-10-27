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

/// `libc::SYS_<name>` as a [`Syscall`] named `<name>`: one identifier gives
/// both, so the number and the name cannot disagree.
macro_rules! syscall {
    ($constant:ident) => {
        Syscall::new(libc::$constant, without_sys(stringify!($constant)))
    };
}

const fn without_sys(constant: &'static str) -> &'static str {
    assert!(
        matches!(constant.as_bytes(), [b'S', b'Y', b'S', b'_', _, ..]),
        "a syscall constant is spelled `SYS_<name>`"
    );
    constant.split_at("SYS_".len()).1
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
                syscall!(SYS_ptrace),
                syscall!(SYS_process_vm_readv),
                syscall!(SYS_process_vm_writev),
                syscall!(SYS_pidfd_getfd),
                syscall!(SYS_process_madvise),
                syscall!(SYS_process_mrelease),
                syscall!(SYS_kcmp),
            ],
            when: None,
            verdict: Verdict::Kill,
            why: "reads or rewrites another process; the envelope hides the host's table, \
                  this closes the same-uid pair inside it",
        },
        Rule {
            calls: &[
                syscall!(SYS_perf_event_open),
                syscall!(SYS_bpf),
                #[cfg(target_arch = "x86_64")]
                syscall!(SYS_modify_ldt),
            ],
            when: None,
            verdict: Verdict::Kill,
            why: "loads programs into the kernel; classic privilege-escalation surface",
        },
        Rule {
            calls: &[
                syscall!(SYS_kexec_load),
                // The local `SYS_KEXEC_FILE_LOAD`, not a libc `SYS_<name>`.
                Syscall::new(SYS_KEXEC_FILE_LOAD, "kexec_file_load"),
                syscall!(SYS_reboot),
                syscall!(SYS_swapon),
                syscall!(SYS_swapoff),
            ],
            when: None,
            verdict: Verdict::Kill,
            why: "machine state",
        },
        Rule {
            calls: &[
                syscall!(SYS_init_module),
                syscall!(SYS_finit_module),
                syscall!(SYS_delete_module),
            ],
            when: None,
            verdict: Verdict::Kill,
            why: "kernel code",
        },
        Rule {
            calls: &[
                syscall!(SYS_keyctl),
                syscall!(SYS_add_key),
                syscall!(SYS_request_key),
            ],
            when: None,
            verdict: Verdict::Kill,
            why: "the kernel keyring: cross-process state the envelope does not model",
        },
        Rule {
            calls: &[syscall!(SYS_userfaultfd)],
            when: None,
            verdict: Verdict::Kill,
            why: "the race-widening primitive in most modern exploit chains",
        },
        Rule {
            calls: &[syscall!(SYS_open_by_handle_at)],
            when: None,
            verdict: Verdict::Kill,
            why: "opens by inode handle, bypassing every path the envelope bound",
        },
        Rule {
            calls: &[
                syscall!(SYS_mount),
                syscall!(SYS_umount2),
                syscall!(SYS_pivot_root),
                syscall!(SYS_move_mount),
                syscall!(SYS_open_tree),
                syscall!(SYS_fsopen),
                syscall!(SYS_fsmount),
                syscall!(SYS_fsconfig),
                syscall!(SYS_fspick),
                syscall!(SYS_mount_setattr),
            ],
            when: None,
            verdict: Verdict::Kill,
            why: "mount authority inside the envelope hides or reshapes the deny masks",
        },
        Rule {
            calls: &[syscall!(SYS_unshare), syscall!(SYS_clone)],
            when: Some(Cond::FlagSet {
                arg: 0,
                mask: CLONE_NEWUSER,
            }),
            verdict: Verdict::Errno(libc::EPERM),
            why: "a user namespace is root over a fresh mount tree: a container inside the \
                  grant, which the envelope forbids",
        },
        Rule {
            calls: &[syscall!(SYS_setns)],
            when: None,
            verdict: Verdict::Errno(libc::EPERM),
            why: "joining another namespace is leaving this one",
        },
        Rule {
            calls: &[syscall!(SYS_clone3)],
            when: None,
            verdict: Verdict::Errno(libc::ENOSYS),
            why: "its flags live in a struct seccomp cannot read; ENOSYS makes glibc and Rust \
                  std fall back to `clone`, which the row above inspects (Docker's default \
                  profile does the same)",
        },
        Rule {
            calls: &[syscall!(SYS_ioctl)],
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
mod tests;
