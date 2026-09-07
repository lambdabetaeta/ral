//! The process sandbox a confined payload enters *inside* the bwrap envelope:
//! kernel exec confinement rendering `ExecProjection::Restricted`, and a
//! signal scope closing the same-uid `kill` hole where the host cannot build
//! a pid namespace.  Entered by the payload itself, never before bwrap: a
//! domain handling any fs right forbids `mount(2)`, bwrap's first act.
//!
//! Declared gap: Landlock is allow-list only, so `deny_paths`, `deny_dirs`
//! and `deny_basenames` render nothing here — a deny outside every admit is
//! already absence, and a deny *inside* an admit stays with the in-ral gate
//! on Linux, where Seatbelt would carry it into the kernel.

use crate::path::{Rendered, render_paths};
use crate::types::{ExecProjection, SandboxProjection};
use std::fmt;
use std::io;
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

/// The rendered ruleset as a value, so it can be tested on any host and
/// printed in the profile dump without touching the kernel.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Layer {
    /// `Some` renders `ExecProjection::Restricted`.
    exec: Option<Exec>,
    scope_signals: bool,
}

/// The exec half: every admit carries `Execute` and nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Exec {
    admits: Vec<Rendered>,
    /// `Refer` handled and granted on `/`: a domain that does not refuses every
    /// cross-directory rename and link (EXDEV), which a layer about exec must
    /// not do.  Grantable from ABI 2.
    frees_refer: bool,
}

impl Layer {
    /// The layer `policy` asks of a kernel at `abi`, or `None` where there is
    /// nothing to enter.
    fn for_policy(policy: &SandboxProjection, abi: Abi) -> Result<Option<Self>, String> {
        Self::render(&policy.rendered()?.exec, abi)
    }

    /// # Errors
    /// A loader or self path this host cannot spell in Unicode: an admit is
    /// never approximated, and dropping it silently would deny every exec
    /// with a bare `EACCES`.
    fn render(exec: &ExecProjection<Rendered>, abi: Abi) -> Result<Option<Self>, String> {
        let scope_signals = abi >= Abi::SIGNAL_SCOPE;
        let exec = match exec {
            ExecProjection::Unrestricted => None,
            ExecProjection::Restricted {
                allow_paths,
                allow_dirs,
                ..
            } => {
                let base: Vec<String> = platform_base().into_iter().chain([self_path()?]).collect();
                let mut admits: Vec<Rendered> = allow_paths
                    .iter()
                    .chain(allow_dirs)
                    .cloned()
                    .chain(render_paths(&base)?)
                    .filter(|p| crate::path::exists(p.as_str()))
                    .collect();
                admits.sort();
                admits.dedup();
                Some(Exec {
                    admits,
                    frees_refer: abi >= Abi::REFER,
                })
            }
        };
        Ok((exec.is_some() || scope_signals).then_some(Self {
            exec,
            scope_signals,
        }))
    }

    /// Apply the layer to the current process, fail-closed: a partially
    /// enforced exec layer is a confinement nobody asked for.
    fn enter(&self) -> Result<(), Error> {
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
        if let Some(exec) = &self.exec {
            if exec.frees_refer {
                created = admit(created, "/", AccessFs::Refer)?;
            }
            for path in &exec.admits {
                created = admit(created, path.as_str(), AccessFs::Execute)?;
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
    path: &str,
    access: landlock::AccessFs,
) -> Result<landlock::RulesetCreated, Error> {
    use landlock::RulesetCreatedAttr;
    created
        .add_rule(landlock::PathBeneath::new(open(path)?, access))
        .map_err(|source| Error::AddRule {
            path: path.to_string(),
            source,
        })
}

/// `statfs.f_type` of a 9P mount, whose server implements no Landlock hook.
const V9FS_MAGIC: u64 = 0x0102_1997;

impl fmt::Display for Layer {
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

fn on_9p(path: &str) -> bool {
    rustix::fs::statfs(path).is_ok_and(|s| u64::try_from(s.f_type).is_ok_and(|t| t == V9FS_MAGIC))
}

fn open(path: &str) -> Result<landlock::PathFd, Error> {
    landlock::PathFd::new(path).map_err(|e| Error::OpenAdmit {
        path: path.to_string(),
        source: match e {
            landlock::PathFdError::OpenCall { source, .. } => source,
            // The crate's enum is non_exhaustive; any future variant still means the open failed.
            other => io::Error::other(other.to_string()),
        },
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
/// `Execute` on the binary and on its `PT_INTERP` file; the shared libraries
/// the loader then maps need no right from this layer.  So the base is a set
/// of regular files — never a directory, which under `/usr/bin` would be a
/// layer that denies nothing.
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
        path: String,
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
            Self::AddRule { path, source } => write!(
                f,
                "landlock: the kernel refused the rule admitting {path}: {source}"
            ),
            Self::OpenAdmit { path, source } => write!(
                f,
                "landlock: {path} could not be opened: {source}. It existed when the \
                 layer was rendered, so this is a race with something removing it, \
                 not a policy error."
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

/// Enter the layer `policy` asks for, in the current process.  A kernel with
/// no Landlock enters nothing: absence is a declared, unheld invariant, and
/// there is no layer to apply.
pub(crate) fn enter(policy: &SandboxProjection) -> Result<(), String> {
    let abi = match Landlock::probe() {
        Landlock::Absent => return Ok(()),
        unprobed @ Landlock::Unprobed(_) => {
            return Err(format!("landlock: {unprobed}; refusing to run confined"));
        }
        Landlock::At(abi) => abi,
    };
    match Layer::for_policy(policy, abi)? {
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
    match Layer::for_policy(policy, abi) {
        Ok(Some(layer)) => eprintln!("--- landlock layer ---\n{layer}--- end landlock layer ---"),
        Ok(None) => eprintln!("landlock layer: none (nothing to enter)"),
        Err(e) => eprintln!("--- landlock layer error ---\n{e}"),
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs scaffolding"
)]
mod tests {
    use super::*;
    use std::io::Write;

    fn restricted(allow_paths: Vec<&str>, denies: bool) -> ExecProjection<Rendered> {
        let render = |v: Vec<&str>| render_paths(&v).expect("ASCII paths render");
        ExecProjection::Restricted {
            allow_paths: render(allow_paths),
            allow_dirs: Vec::new(),
            deny_paths: if denies {
                render(vec!["/usr/bin/curl"])
            } else {
                Vec::new()
            },
            deny_dirs: if denies {
                render(vec!["/usr/local/bin"])
            } else {
                Vec::new()
            },
            deny_basenames: if denies {
                vec!["curl".to_string()]
            } else {
                Vec::new()
            },
        }
    }

    fn layer(exec: &ExecProjection<Rendered>, abi: Abi) -> Option<Layer> {
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
    fn the_deny_sets_render_nothing() {
        let with = layer(&restricted(vec!["/bin/true"], true), Abi(9));
        let without = layer(&restricted(vec!["/bin/true"], false), Abi(9));
        assert_eq!(
            with, without,
            "Landlock is allow-list only; the denies stay with the in-ral gate"
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
