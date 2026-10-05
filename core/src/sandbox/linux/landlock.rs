//! The process sandbox a confined payload enters *inside* the bwrap envelope:
//! kernel exec confinement rendering `ExecProjection::Restricted`, and a
//! signal scope closing the same-uid `kill` hole where the host cannot build
//! a pid namespace.  Entered by the payload itself, never before bwrap: a
//! domain handling any fs right forbids `mount(2)`, bwrap's first act.  Its
//! admits are opened by the parent, in the host, never through a symlink
//! ([`prepare`]); the payload inherits them as fds ([`Entry`]).
//!
//! Declared gap: Landlock is allow-list only and cannot remove part of an
//! allowed directory, so denies and vetoes render nothing here — a deny
//! outside every allow is already absence, and a deny inside an allowed
//! directory holds only at the in-process guard on Linux, where Seatbelt
//! would carry it into the kernel.

use crate::path::{Rendered, render_paths, render_real};
use crate::types::{ExecProjection, ExecRule, SandboxProjection};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io;
use std::os::fd::OwnedFd;
use std::sync::OnceLock;

/// A Landlock ABI level, as `landlock_create_ruleset` reports it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Abi(u32);

impl Abi {
    /// `LANDLOCK_ACCESS_FS_EXECUTE`, Linux 5.13.
    pub(crate) const EXEC: Self = Self(1);
    /// `LANDLOCK_ACCESS_FS_REFER`, Linux 5.19.
    pub(crate) const REFER: Self = Self(2);
    /// `LANDLOCK_SCOPE_SIGNAL`, Linux 6.12.
    pub(crate) const SIGNAL_SCOPE: Self = Self(6);
}

impl fmt::Display for Abi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What the version probe answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Landlock {
    /// Not built in (`ENOSYS`) or absent from the boot LSM list (`EOPNOTSUPP`).
    Absent,
    At(Abi),
    /// Any other errno: not a kernel without Landlock, so never treated as one.
    Unprobed(i32),
}

impl Landlock {
    /// The syscall is the only honest source: a version is not a feature list,
    /// so `uname` is never read.
    pub(crate) fn probe() -> Self {
        /// Not exported by `libc`; `LANDLOCK_CREATE_RULESET_VERSION` in the UAPI.
        const VERSION: libc::c_ulong = 1;
        static PROBED: OnceLock<Landlock> = OnceLock::new();
        *PROBED.get_or_init(|| {
            // SAFETY: the version query takes a null attr and a zero size.
            let level = unsafe {
                libc::syscall(
                    libc::SYS_landlock_create_ruleset,
                    std::ptr::null::<libc::c_void>(),
                    0usize,
                    VERSION,
                )
            };
            let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            classify(level, errno)
        })
    }

    pub(crate) fn abi(self) -> Option<Abi> {
        match self {
            Self::At(abi) => Some(abi),
            _ => None,
        }
    }
}

fn classify(level: libc::c_long, errno: i32) -> Landlock {
    match u32::try_from(level) {
        Ok(level) => Landlock::At(Abi(level)),
        Err(_) if errno == libc::ENOSYS || errno == libc::EOPNOTSUPP => Landlock::Absent,
        Err(_) => Landlock::Unprobed(errno),
    }
}

impl fmt::Display for Landlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => write!(
                f,
                "this kernel has no Landlock (not built in, or absent from the boot LSM list)"
            ),
            Self::At(abi) => write!(f, "this kernel's Landlock is ABI {abi}"),
            Self::Unprobed(errno) => write!(
                f,
                "the Landlock version probe failed: {}, which is not a kernel without it",
                io::Error::from_raw_os_error(*errno)
            ),
        }
    }
}

/// Landlock carries the exec allow-list into the kernel via the `Execute`
/// ruleset [`Layer::enter`] builds below.
pub(crate) const RENDERS_EXEC: bool = true;

/// What crosses into the envelope for the trampoline to enter.  Never a
/// path: every name in there is one bwrap minted by following host symlinks,
/// so none can be trusted to reach the file a grant froze.  The parent opens
/// each admit in the host instead, and the payload inherits them as fds from
/// [`super::ADMIT_FD_BASE`].
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Entry {
    /// `Some(n)`: exec is confined to the `n` inherited admits.
    exec_admits: Option<usize>,
}

/// The ruleset as a value, admits named by `A`: rendered paths in the parent,
/// so it can be tested on any host and printed in the profile dump, and the
/// inherited fds in the payload, which enters it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Layer<A> {
    /// `Some` renders `ExecProjection::Restricted`.
    exec: Option<Exec<A>>,
    scope_signals: bool,
}

/// The exec half: every admit carries `Execute` and nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Exec<A> {
    admits: Vec<A>,
    /// `Refer` handled and granted on `/`: a domain that does not refuses every
    /// cross-directory rename and link (EXDEV), which a layer about exec must
    /// not do.  Grantable from ABI 2.
    frees_refer: bool,
}

impl<A> Layer<A> {
    /// The layer a kernel at `abi` enters, exec confined to `admits` unless
    /// `None`; `None` where there is nothing to enter.
    fn at(admits: Option<Vec<A>>, abi: Abi) -> Option<Self> {
        let scope_signals = abi >= Abi::SIGNAL_SCOPE;
        let exec = admits.map(|admits| Exec {
            admits,
            frees_refer: abi >= Abi::REFER,
        });
        (exec.is_some() || scope_signals).then_some(Self {
            exec,
            scope_signals,
        })
    }
}

impl Layer<Rendered> {
    /// The layer `exec` asks of a kernel at `abi`.
    fn render(exec: &ExecProjection, abi: Abi) -> Result<Option<Self>, String> {
        Ok(Self::at(admits(exec)?, abi))
    }
}

/// Every allow rule — a dir as a hierarchy, a file as itself, `Execute`
/// beneath either — plus the loader base and ral's own binary; `None` when
/// exec is unrestricted.  Real spellings only: a rule attaches to the inode
/// its open reaches, and [`open`] follows no symlink.
///
/// # Errors
/// A path this host cannot spell in Unicode: an admit is never
/// approximated, and dropping it silently would deny every exec with a bare
/// `EACCES`.
fn admits(exec: &ExecProjection) -> Result<Option<Vec<Rendered>>, String> {
    let ExecProjection::Restricted(rules) = exec else {
        return Ok(None);
    };
    let base: Vec<String> = platform_base().into_iter().chain([self_path()?]).collect();
    let mut admits = render_paths(&base)?;
    for rule in rules {
        if let ExecRule::Dir { path, allow: true } | ExecRule::File { path, allow: true } = rule {
            admits.extend(render_real(path)?);
        }
    }
    admits.retain(|p| is_real(p.as_str()));
    admits.sort();
    admits.dedup();
    Ok(Some(admits))
}

impl Layer<OwnedFd> {
    /// Apply the layer to the current process, fail-closed: a partially
    /// enforced exec layer is a confinement nobody asked for.
    fn enter(self) -> Result<(), Error> {
        use landlock::{
            AccessFs, BitFlags, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetStatus, Scope,
        };

        // Every handled right is a hard requirement: there is nothing to degrade to.
        let mut ruleset = Ruleset::default().set_compatibility(CompatLevel::HardRequirement);
        if let Some(exec) = &self.exec {
            let mut handled: BitFlags<AccessFs> = AccessFs::Execute.into();
            if exec.frees_refer {
                handled |= AccessFs::Refer;
            }
            ruleset = ruleset
                .handle_access(handled)
                .map_err(Error::CreateRuleset)?;
        }
        if self.scope_signals {
            ruleset = ruleset.scope(Scope::Signal).map_err(Error::CreateRuleset)?;
        }
        let mut created = ruleset
            .create()
            .map_err(Error::CreateRuleset)?
            .set_compatibility(CompatLevel::HardRequirement);
        if let Some(exec) = self.exec {
            // The envelope's own root, which exists only in here.
            if exec.frees_refer {
                created = admit(created, open("/")?, AccessFs::Refer, "/")?;
            }
            for fd in exec.admits {
                created = admit(created, fd, AccessFs::Execute, "an inherited exec admit")?;
            }
        }
        let status = created.restrict_self().map_err(Error::RestrictSelf)?;
        if status.ruleset == RulesetStatus::FullyEnforced {
            Ok(())
        } else {
            Err(Error::PartiallyEnforced)
        }
    }
}

fn admit(
    created: landlock::RulesetCreated,
    fd: OwnedFd,
    access: landlock::AccessFs,
    what: &str,
) -> Result<landlock::RulesetCreated, Error> {
    use landlock::RulesetCreatedAttr;
    created
        .add_rule(landlock::PathBeneath::new(fd, access))
        .map_err(|source| Error::AddRule {
            admit: what.to_string(),
            source,
        })
}

/// The entry for a launch under `policy`, and the admits it counts, opened
/// here in the host.  A kernel without Landlock gets no exec layer.
///
/// # Errors
/// As [`admits`], or an admit that stopped being a real path since.
pub(crate) fn prepare(
    policy: &SandboxProjection,
    landlock: Landlock,
) -> Result<(Entry, Vec<OwnedFd>), String> {
    let admits = match landlock.abi() {
        Some(_) => admits(&policy.exec)?,
        None => None,
    };
    let fds = admits
        .iter()
        .flatten()
        .map(|path| open(path.as_str()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let entry = Entry {
        exec_admits: admits.map(|admits| admits.len()),
    };
    Ok((entry, fds))
}

/// Take ownership of the `n` admits the parent installed, so they close
/// before the payload becomes its target.
fn inherited(n: usize) -> Result<Vec<OwnedFd>, String> {
    use std::os::fd::FromRawFd;
    (0..n)
        .map(|i| {
            let fd = libc::c_int::try_from(i)
                .ok()
                .and_then(|i| super::ADMIT_FD_BASE.checked_add(i))
                .ok_or("landlock: too many exec admits to inherit")?;
            // SAFETY: open, by F_GETFD, and owned by nothing else in this
            // process: the parent installed it for this call alone.
            if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
                return Err(format!(
                    "landlock: exec admit fd {fd} was not inherited; refusing to run confined"
                ));
            }
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        })
        .collect()
}

/// `statfs.f_type` of a 9P mount, whose server implements no Landlock hook.
const V9FS_MAGIC: u64 = 0x0102_1997;

impl fmt::Display for Layer<Rendered> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.exec {
            None => writeln!(f, "landlock: exec unconfined (projection unrestricted)")?,
            Some(exec) => {
                writeln!(f, "landlock: exec confined to {} paths:", exec.admits.len())?;
                for admit in &exec.admits {
                    let note = if on_9p(admit.as_str()) {
                        " (inert: 9P implements no Landlock hooks)"
                    } else {
                        ""
                    };
                    writeln!(f, "  {}{note}", admit.as_str())?;
                }
                writeln!(
                    f,
                    "cross-directory rename freed (Refer on /): {}",
                    if exec.frees_refer {
                        "yes"
                    } else {
                        "no (ABI 1)"
                    }
                )?;
            }
        }
        writeln!(
            f,
            "signals scoped to the envelope: {}",
            if self.scope_signals { "yes" } else { "no" }
        )
    }
}

/// Landlock attaches to the inode a path reaches, so only a path that is its
/// own real path may be admitted.
#[allow(clippy::disallowed_methods)]
fn is_real(path: &str) -> bool {
    crate::path::canon::canonicalise_strict(std::path::Path::new(path))
        .is_ok_and(|real| real == std::path::Path::new(path))
}

fn on_9p(path: &str) -> bool {
    rustix::fs::statfs(path).is_ok_and(|s| u64::try_from(s.f_type).is_ok_and(|t| t == V9FS_MAGIC))
}

/// Never through a symlink: a rule attaches to the inode the open reaches.
fn open(path: &str) -> Result<OwnedFd, Error> {
    use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, openat2};
    openat2(
        CWD,
        path,
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS,
    )
    .map_err(|errno| Error::OpenAdmit {
        path: path.to_string(),
        source: io::Error::from(errno),
    })
}

/// The running binary, admitted unconditionally so a bundled-tool re-exec
/// need not be named by every policy — as macOS admits its own exec path.
fn self_path() -> Result<String, String> {
    let path = super::super::reexec::self_arg0()
        .map_err(|e| format!("landlock: cannot resolve ral's own path: {e}"))?;
    path.into_os_string().into_string().map_err(|p| {
        format!(
            "landlock: ral's own path {} is not Unicode, so it cannot be admitted",
            p.display()
        )
    })
}

/// The dynamic linkers, and nothing else.  `execve` of a dynamic binary needs
/// `Execute` on the binary and on its `PT_INTERP` file — and of a `#!` script,
/// on its interpreter, which is a carrier rather than base; the shared
/// libraries the loader then maps need no right from this layer.  So the base
/// is a set of regular files — never a directory, which under `/usr/bin`
/// would be a layer that denies nothing.
fn platform_base() -> Vec<String> {
    const PATTERNS: &[&str] = &[
        "/lib/ld*.so*",
        "/lib64/ld*.so*",
        "/lib32/ld*.so*",
        "/usr/lib/ld*.so*",
        "/usr/lib64/ld*.so*",
        "/usr/lib32/ld*.so*",
        // Debian multiarch, e.g. /usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1.
        "/lib/*/ld*.so*",
        "/usr/lib/*/ld*.so*",
    ];
    let mut found: Vec<String> = PATTERNS
        .iter()
        .filter_map(|p| glob::glob(p).ok())
        .flatten()
        .flatten()
        // Merged-/usr makes /lib/x and /usr/lib/x one inode; canonicalise so
        // the two spellings collapse to one rule.
        .filter_map(|p| crate::path::canon::canonicalise_strict(&p).ok())
        .filter(|p| p.is_file())
        .filter_map(|p| p.into_os_string().into_string().ok())
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Which stage refused, and what the user can do about it.
#[derive(Debug)]
pub(crate) enum Error {
    CreateRuleset(landlock::RulesetError),
    AddRule {
        admit: String,
        source: landlock::RulesetError,
    },
    OpenAdmit {
        path: String,
        source: io::Error,
    },
    RestrictSelf(landlock::RulesetError),
    PartiallyEnforced,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateRuleset(e) => {
                write!(f, "landlock: this kernel refused the ruleset: {e}")
            }
            Self::AddRule { admit, source } => write!(
                f,
                "landlock: the kernel refused the rule admitting {admit}: {source}"
            ),
            Self::OpenAdmit { path, source } => write!(
                f,
                "landlock: {path} could not be opened without following a symlink: \
                 {source}. It was a real path when the layer was rendered, so something \
                 removed or replaced it since: a race, not a policy error."
            ),
            Self::RestrictSelf(e) => write!(f, "landlock: landlock_restrict_self failed: {e}"),
            Self::PartiallyEnforced => write!(
                f,
                "landlock: the kernel enforced only part of the layer, which would leave \
                 it half-applied; refusing to run confined."
            ),
        }
    }
}

/// Enter the layer `entry` describes, in the current process.  A kernel with
/// no Landlock enters nothing: absence is a declared, unheld invariant, and
/// there is no layer to apply.
pub(crate) fn enter(entry: &Entry) -> Result<(), String> {
    let admits = entry.exec_admits.map(inherited).transpose()?;
    let abi = match Landlock::probe() {
        Landlock::Absent => return Ok(()),
        unprobed @ Landlock::Unprobed(_) => {
            return Err(format!("landlock: {unprobed}; refusing to run confined"));
        }
        Landlock::At(abi) => abi,
    };
    match Layer::at(admits, abi) {
        Some(layer) => layer.enter().map_err(|e| e.to_string()),
        None => Ok(()),
    }
}

/// The layer the trampoline would enter on this kernel, for the profile dump:
/// entered by a trampoline, it is otherwise invisible in the bwrap argv.
pub(crate) fn dump(policy: &SandboxProjection, landlock: Landlock) {
    let abi = match landlock {
        Landlock::Absent => {
            eprintln!("landlock layer: none (no Landlock on this kernel)");
            return;
        }
        unprobed @ Landlock::Unprobed(_) => {
            eprintln!("landlock layer: unknown — {unprobed}; a confined launch refuses");
            return;
        }
        Landlock::At(abi) => abi,
    };
    match Layer::render(&policy.exec, abi) {
        Ok(Some(layer)) => eprintln!("--- landlock layer ---\n{layer}--- end landlock layer ---"),
        Ok(None) => eprintln!("landlock layer: none (nothing to enter)"),
        Err(e) => eprintln!("--- landlock layer error ---\n{e}"),
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "[test] test fs scaffolding")]
mod tests {
    use super::*;
    use std::io::Write;

    fn restricted(allow_paths: Vec<&str>, denies: bool) -> ExecProjection {
        let real = crate::path::RealPath::assumed;
        let mut rules: Vec<ExecRule> = allow_paths
            .into_iter()
            .map(|path| ExecRule::File {
                path: real(path),
                allow: true,
            })
            .collect();
        if denies {
            rules.push(ExecRule::Dir {
                path: real("/usr/local/bin"),
                allow: false,
            });
            rules.push(ExecRule::File {
                path: real("/usr/bin/curl"),
                allow: false,
            });
            rules.push(ExecRule::Veto("curl".to_string()));
        }
        ExecProjection::Restricted(rules)
    }

    fn layer(exec: &ExecProjection, abi: Abi) -> Option<Layer<Rendered>> {
        Layer::render(exec, abi).expect("ASCII paths render")
    }

    #[test]
    fn only_the_two_absence_errnos_read_as_a_kernel_without_landlock() {
        assert_eq!(classify(5, 0), Landlock::At(Abi(5)));
        assert_eq!(classify(-1, libc::ENOSYS), Landlock::Absent);
        assert_eq!(classify(-1, libc::EOPNOTSUPP), Landlock::Absent);
        assert_eq!(
            classify(-1, libc::EPERM),
            Landlock::Unprobed(libc::EPERM),
            "a seccomp EPERM is not absence"
        );
    }

    #[test]
    fn an_unrestricted_projection_asks_for_a_layer_only_where_the_scope_exists() {
        for abi in [Abi::EXEC, Abi::REFER, Abi(5)] {
            assert_eq!(
                layer(&ExecProjection::Unrestricted, abi),
                None,
                "abi {abi} has no signal scope, so an unrestricted projection has nothing to enter"
            );
        }
        for abi in [Abi::SIGNAL_SCOPE, Abi(9)] {
            let scope = layer(&ExecProjection::Unrestricted, abi).expect("a scope layer");
            assert_eq!(scope.exec, None);
            assert!(scope.scope_signals);
        }
    }

    #[test]
    fn a_restricted_projection_frees_refer_from_abi_two_and_scopes_from_six() {
        for (abi, frees_refer, scope_signals) in [
            (Abi::EXEC, false, false),
            (Abi::REFER, true, false),
            (Abi(5), true, false),
            (Abi::SIGNAL_SCOPE, true, true),
            (Abi(9), true, true),
        ] {
            let rendered = layer(&restricted(vec!["/bin/true"], false), abi)
                .expect("a restricted projection always asks for an exec layer");
            let exec = rendered
                .exec
                .unwrap_or_else(|| panic!("abi {abi} must confine exec"));
            assert_eq!(exec.frees_refer, frees_refer, "abi {abi}");
            assert_eq!(rendered.scope_signals, scope_signals, "abi {abi}");
        }
    }

    #[test]
    fn the_running_binary_is_always_admitted() {
        let admits = layer(&restricted(Vec::new(), false), Abi(9))
            .and_then(|l| l.exec)
            .expect("exec admits")
            .admits;
        let own = super::super::super::reexec::self_arg0().expect("own path");
        assert!(
            admits.iter().any(|a| a.as_str() == own.to_string_lossy()),
            "a bundled-tool re-exec must not need naming in the policy: {admits:?}"
        );
    }

    #[test]
    fn an_admit_is_kept_where_it_exists_and_dropped_where_it_does_not() {
        let mut kept = tempfile::NamedTempFile::new().expect("temp file");
        kept.write_all(b"#!/bin/sh\n").expect("write");
        let kept = kept.path().to_string_lossy().into_owned();
        let gone = "/nonexistent-ral-landlock-admit";

        let admits = layer(&restricted(vec![&kept, gone], false), Abi(9))
            .and_then(|l| l.exec)
            .expect("exec admits")
            .admits;
        assert!(
            admits.iter().any(|a| a.as_str() == kept),
            "an existing admit must survive: {admits:?}"
        );
        assert!(
            !admits.iter().any(|a| a.as_str() == gone),
            "an admit that names nothing cannot be opened at entry: {admits:?}"
        );
    }

    #[test]
    fn an_admit_that_is_now_a_symlink_is_not_admitted() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).expect("outside");
        let allowed = tmp.path().join("allowed");
        std::os::unix::fs::symlink(&outside, &allowed).expect("symlink");
        let allowed = allowed.to_string_lossy().into_owned();

        let admits = layer(&restricted(vec![&allowed], false), Abi(9))
            .and_then(|l| l.exec)
            .expect("exec admits")
            .admits;
        assert!(
            !admits.iter().any(|a| a.as_str() == allowed),
            "a symlink would admit its target: {admits:?}"
        );
    }

    #[test]
    fn denies_and_vetoes_render_nothing() {
        let with = layer(&restricted(vec!["/bin/true"], true), Abi(9));
        let without = layer(&restricted(vec!["/bin/true"], false), Abi(9));
        assert_eq!(
            with, without,
            "Landlock is allow-list only; the denies stay with the in-process guard"
        );
    }

    #[test]
    fn the_platform_base_is_linker_files_and_never_a_command_directory() {
        let base = platform_base();
        assert!(
            !base.is_empty(),
            "a Linux host runs dynamic binaries, so it has a loader"
        );
        for entry in &base {
            let path = std::path::Path::new(entry);
            assert!(path.is_file(), "{entry} is not a regular file");
            assert!(
                path.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("ld")),
                "{entry} is not a linker"
            );
            for dir in [
                "/bin/",
                "/usr/bin/",
                "/sbin/",
                "/usr/sbin/",
                "/usr/local/bin/",
            ] {
                assert!(
                    !entry.starts_with(dir),
                    "{entry} would make the base admit a whole command directory"
                );
            }
        }
    }
}
