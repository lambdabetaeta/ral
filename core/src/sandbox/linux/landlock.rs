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

    /// This kernel's level, or `None` where Landlock is not built in (ENOSYS)
    /// or absent from the boot LSM list (EOPNOTSUPP).  The syscall is the only
    /// honest source: a version is not a feature list, so `uname` is never read.
    pub(crate) fn probe() -> Option<Self> {
        /// Not exported by `libc`; `LANDLOCK_CREATE_RULESET_VERSION` in the UAPI.
        const VERSION: libc::c_ulong = 1;
        static PROBED: OnceLock<Option<Abi>> = OnceLock::new();
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
            u32::try_from(level).ok().map(Self)
        })
    }
}

impl fmt::Display for Abi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The rendered ruleset as a value, so it can be tested on any host and
/// printed in the profile dump without touching the kernel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Layer {
    /// `Some` renders `ExecProjection::Restricted`: every path carries `Execute` and nothing else.
    exec: Option<Vec<Rendered>>,
    /// `Refer` handled and granted on `/`.  From ABI 2 a domain that does not
    /// handle it refuses every cross-directory rename and link (EXDEV), which
    /// a layer about exec must not do.
    frees_refer: bool,
    scope_signals: bool,
}

impl Layer {
    /// The layer `exec` asks of a kernel at `abi`, or `None` where there is
    /// nothing left to enter.
    pub(crate) fn render(exec: &ExecProjection<Rendered>, abi: Abi) -> Option<Self> {
        let scope_signals = abi >= Abi::SIGNAL_SCOPE;
        match exec {
            ExecProjection::Unrestricted => scope_signals.then_some(Self {
                exec: None,
                frees_refer: false,
                scope_signals,
            }),
            ExecProjection::Restricted {
                allow_paths,
                allow_dirs,
                ..
            } => {
                let mut admits: Vec<Rendered> = allow_paths
                    .iter()
                    .chain(allow_dirs)
                    .cloned()
                    .chain(render_outside(&platform_base()))
                    .chain(render_outside(&self_path()))
                    .filter(|p| crate::path::exists(p.as_str()))
                    .collect();
                admits.sort();
                admits.dedup();
                Some(Self {
                    exec: Some(admits),
                    frees_refer: abi >= Abi::REFER,
                    scope_signals,
                })
            }
        }
    }

    /// Apply the layer to the current process, fail-closed: a partially
    /// enforced exec layer is a confinement nobody asked for.
    pub(crate) fn enter(&self) -> Result<(), Error> {
        use landlock::{
            AccessFs, CompatLevel, Compatible, PathBeneath, Ruleset, RulesetAttr,
            RulesetCreatedAttr, RulesetStatus, Scope,
        };

        // One handled right and no fallback: there is nothing to degrade to.
        let mut ruleset = Ruleset::default().set_compatibility(CompatLevel::HardRequirement);
        if self.exec.is_some() {
            ruleset = ruleset
                .handle_access(AccessFs::Execute)
                .map_err(Error::CreateRuleset)?;
            if self.frees_refer {
                ruleset = ruleset
                    .handle_access(AccessFs::Refer)
                    .map_err(Error::CreateRuleset)?;
            }
        }
        if self.scope_signals {
            ruleset = ruleset
                .scope(Scope::Signal)
                .map_err(Error::CreateRuleset)?;
        }
        let mut created = ruleset
            .create()
            .map_err(Error::CreateRuleset)?
            .set_compatibility(CompatLevel::HardRequirement);
        if self.frees_refer {
            created = created
                .add_rule(PathBeneath::new(open("/")?, AccessFs::Refer))
                .map_err(|source| Error::AddRule {
                    path: "/".to_string(),
                    source,
                })?;
        }
        for admit in self.exec.iter().flatten() {
            created = created
                .add_rule(PathBeneath::new(open(admit.as_str())?, AccessFs::Execute))
                .map_err(|source| Error::AddRule {
                    path: admit.as_str().to_string(),
                    source,
                })?;
        }
        let status = created.restrict_self().map_err(Error::RestrictSelf)?;
        if status.ruleset == RulesetStatus::FullyEnforced {
            Ok(())
        } else {
            Err(Error::PartiallyEnforced)
        }
    }
}

/// `statfs.f_type` of a 9P mount, whose server implements no Landlock hook.
const V9FS_MAGIC: u64 = 0x0102_1997;

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.exec {
            None => writeln!(f, "landlock: exec unconfined (projection unrestricted)")?,
            Some(admits) => {
                writeln!(f, "landlock: exec confined to {} paths:", admits.len())?;
                for admit in admits {
                    let note = if on_9p(admit.as_str()) {
                        " (inert: 9P implements no Landlock hooks)"
                    } else {
                        ""
                    };
                    writeln!(f, "  {}{note}", admit.as_str())?;
                }
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

/// Names outside the projection, so nothing has expanded them yet.  A name
/// this host cannot spell in Unicode drops rather than being approximated:
/// an admit that is missing denies, never widens.
fn render_outside(paths: &[String]) -> Vec<Rendered> {
    render_paths(paths).unwrap_or_default()
}

/// The running binary, admitted unconditionally so a bundled-tool re-exec
/// need not be named by every policy — as macOS admits its own exec path.
fn self_path() -> Vec<String> {
    super::super::reexec::self_arg0()
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok())
        .into_iter()
        .collect()
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
        .filter_map(|p| p.canonicalize().ok())
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
            Self::CreateRuleset(e) => write!(
                f,
                "could not restrict exec: this kernel refused the Landlock ruleset: {e}"
            ),
            Self::AddRule { path, source } => write!(
                f,
                "could not restrict exec: the kernel refused the rule admitting {path}: {source}"
            ),
            Self::OpenAdmit { path, source } => write!(
                f,
                "could not restrict exec: {path} could not be opened: {source}. \
                 It existed when the layer was rendered, so this is a race with \
                 something removing it, not a policy error."
            ),
            Self::RestrictSelf(e) => write!(
                f,
                "could not restrict exec: landlock_restrict_self failed: {e}"
            ),
            Self::PartiallyEnforced => write!(
                f,
                "could not restrict exec: the kernel enforced only part of the layer, \
                 which would leave the exec rules half-applied; refusing to run confined."
            ),
        }
    }
}

/// Enter the layer `policy` asks for, in the current process.  A kernel with
/// no Landlock enters nothing: the parent already reported the unheld
/// invariant, and there is no layer to apply.
pub(crate) fn enter(policy: &SandboxProjection) -> Result<(), String> {
    let Some(abi) = Abi::probe() else {
        return Ok(());
    };
    let rendered = policy.rendered()?;
    let Some(layer) = Layer::render(&rendered.exec, abi) else {
        return Ok(());
    };
    layer.enter().map_err(|e| e.to_string())
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

    #[test]
    fn an_unrestricted_projection_asks_for_a_layer_only_where_the_scope_exists() {
        for abi in [Abi::EXEC, Abi::REFER, Abi(5)] {
            assert_eq!(
                Layer::render(&ExecProjection::Unrestricted, abi),
                None,
                "abi {abi} has no signal scope, so an unrestricted projection has nothing to enter"
            );
        }
        for abi in [Abi::SIGNAL_SCOPE, Abi(9)] {
            let layer = Layer::render(&ExecProjection::Unrestricted, abi).expect("a scope layer");
            assert_eq!(layer.exec, None);
            assert!(!layer.frees_refer);
            assert!(layer.scope_signals);
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
            let layer = Layer::render(&restricted(vec!["/bin/true"], false), abi)
                .expect("a restricted projection always asks for an exec layer");
            assert!(layer.exec.is_some(), "abi {abi} must confine exec");
            assert_eq!(layer.frees_refer, frees_refer, "abi {abi}");
            assert_eq!(layer.scope_signals, scope_signals, "abi {abi}");
        }
    }

    #[test]
    fn the_running_binary_is_always_admitted() {
        let layer = Layer::render(&restricted(Vec::new(), false), Abi(9)).expect("a layer");
        let admits = layer.exec.expect("exec admits");
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

        let layer = Layer::render(&restricted(vec![&kept, gone], false), Abi(9)).expect("a layer");
        let admits = layer.exec.expect("exec admits");
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
        let with = Layer::render(&restricted(vec!["/bin/true"], true), Abi(9)).expect("a layer");
        let without =
            Layer::render(&restricted(vec!["/bin/true"], false), Abi(9)).expect("a layer");
        assert_eq!(
            with.exec, without.exec,
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
            for dir in ["/bin/", "/usr/bin/", "/sbin/", "/usr/sbin/", "/usr/local/bin/"] {
                assert!(
                    !entry.starts_with(dir),
                    "{entry} would make the base admit a whole command directory"
                );
            }
        }
    }

    #[test]
    fn this_host_carries_the_signal_scope() {
        let abi = Abi::probe().expect("this host has Landlock");
        assert!(
            abi >= Abi::SIGNAL_SCOPE,
            "the signal scope needs ABI {}, this host reports {abi}",
            Abi::SIGNAL_SCOPE
        );
    }
}
