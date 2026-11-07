//! Capability composition for exarch.
//!
//! ```text
//!   ceiling = base ◁ extend?
//!   stack   = [ceiling, restrict₁, restrict₂, ..., deny(restricts)?, deny(credentials)?, self?]
//! ```
//!
//! One optional extension shapes the ceiling, in its own layer; every other
//! composition is the [`GrantStack`] itself — each `--restrict` file, and
//! each deny carve-out, is pushed as its own layer rather than folded ahead
//! of time, so the stack's own per-check fold is the one meet that ever
//! runs.  Composition is explicit — nothing is auto-loaded.

mod base;
mod load;

use base::resolve_base;
use load::{absolute_in, load_capabilities_ral, load_fragment_ral};
use ral_core::authority::grant::freeze::{Anchor, FreezeCtx, host_absolute};
use ral_core::authority::grant::value::{Axes, ExecTable, FsTable, GrantValue, Target};
use ral_core::authority::policy::{GrantLayer, GrantStack};
use ral_core::error::Error;
use ral_core::host::EnvVars;
use ral_core::io::terminal::TerminalState;
use ral_core::path::{CwdPath, FrozenPath};
use ral_core::shell::grant::decode_fragment;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Compose a session's effective [`GrantStack`], with the restrict files'
/// absolute paths.
///
/// `base_name` selects a bake-in profile from `base`.  Every profile freezes
/// against the session's `HOME`, working directory and `scratch` (the spelling
/// of the directory the host made for the session before any policy froze) as
/// it loads, so the extension and every layer push run on already-resolved
/// bundles.  Wherever
/// some layer's `fs` restricts, each restrict file's own path joins a deny
/// layer, putting the bytes that shape the agent's permissions beyond its
/// reach; the extend file does not, since extending authority is a
/// trust-source concern rather than a containment one.  On the same
/// footing the stack denies
/// [`provider::credential_files`](crate::provider::credential_files) in
/// its own layer.  Last, wherever some layer's exec restricts, a layer
/// excludes exarch's own binary from exec, so the agent cannot start exarch.
/// `own` supplies the binary's boot-pinned path when exec is restricted.
/// A session whose fs and exec are unrestricted gets no exclusion, so
/// `dangerous` is ambient authority and needs no OS sandbox.
///
/// # Errors
/// Unknown `base_name`, a profile that fails to load, or an exarch binary
/// that cannot be named.
pub fn for_invocation(
    cwd: &CwdPath,
    scratch: &str,
    base_name: &str,
    extension: Option<&Path>,
    restrict_files: &[PathBuf],
    own: Option<&Path>,
) -> Result<Invocation, Error> {
    let ctx = FreezeCtx::at(EnvVars::new(), cwd.clone()).with_scratch(Some(scratch));
    let credentials = crate::provider::credential_files();
    compose(
        &ctx,
        base_name,
        extension,
        restrict_files,
        &credentials,
        own,
    )
}

/// [`for_invocation`] against `ctx`, carving out `credentials`.
fn compose(
    ctx: &FreezeCtx,
    base_name: &str,
    extension: Option<&Path>,
    restrict_files: &[PathBuf],
    credentials: &[PathBuf],
    own: Option<&Path>,
) -> Result<Invocation, Error> {
    let cwd = ctx.cwd();
    // The loader evaluates ral source, so it needs a Shell.  This one is
    // scaffolding: the caller builds the session's real shell separately, from
    // the frozen GrantStack returned here.
    let mut load_shell = ral_core::HostSurface::default()
        .shell(TerminalState::default(), cwd.clone())
        .map_err(|e| e.with_context("the capability loader cannot start"))?;

    let mut ceiling: GrantLayer = resolve_base(base_name, ctx)?;
    if let Some(path) = extension {
        let abs = absolute_in(cwd, path);
        ceiling = ceiling.extend(load_fragment_ral(
            &ral_core::shell::runtime::Mooring::adrift(),
            &mut load_shell,
            &abs,
            "--extend",
            ctx,
        )?);
    }

    let mut stack = GrantStack::default();
    stack.push(ceiling);

    let restricts: Vec<PathBuf> = restrict_files.iter().map(|p| absolute_in(cwd, p)).collect();
    for path in &restricts {
        stack.push(load_capabilities_ral(
            &ral_core::shell::runtime::Mooring::adrift(),
            &mut load_shell,
            path,
            "--restrict",
            ctx,
        )?);
    }

    // Our own credentials and the restrict files are the authority a grant is
    // *made of*, never something it hands out, so they are carved out in their
    // own layer, where no profile can forget them and no `--extend` can widen
    // them back.  Only where some layer's fs restricts: an unrestricted
    // session asked not to be confined, and a layer holding these exclusions
    // would confine its fs (on Windows, to "every path less some", which
    // refuses every launch).  An fs-silent restrict file is thus editable by
    // the agent, as an `--extend` file is.
    if !stack.fs_is_top() {
        if !restricts.is_empty() {
            stack.push(deny_layer(&restricts, ctx)?);
        }
        stack.push(deny_layer(credentials, ctx)?);
    }

    // On the same footing: exec unrestricted asked not to be confined, and an
    // exec exclusion would confine every launch, which on Linux needs bwrap.
    // A child a `dangerous` parent narrows thus runs exarch, regaining only the
    // parent's own authority.
    if !stack.exec_is_top() {
        stack.push(self_layer(own, ctx)?);
    }

    Ok(Invocation { stack, restricts })
}

/// A session's composed authority, with the pieces its callers need by name.
#[derive(Debug)]
pub struct Invocation {
    /// The whole meet, the self layer, if any, last.
    pub stack: GrantStack,
    /// Absolute paths of the `--restrict` files.
    pub restricts: Vec<PathBuf>,
}

/// The bake-in bases a spawn's `grant` may name — a subset of what
/// [`base::resolve_base`] offers a launching human, kept in step by hand.
/// `` `inherit `` and `` `restrict `` name no base, so neither appears here.
///
/// Each admits the bundled coreutils (`ral_core::uutils`). Those spawn by bare
/// name, so a base that states its exec admissions as directory prefixes alone
/// denies every one of them, and a child that cannot run `ls` cannot widen its
/// own grant to get it back.
pub(crate) const SPAWN_BASES: [&str; 4] = ["confined", "read-only", "edit-only", "reasonable"];

/// Resolve a bake-in base for a spawned child — the layer a caller pushes onto
/// the parent's stack, and the narrower every spawn grant naming a base
/// resolves through.
///
/// Frozen against the child's working directory and scratch, which `ctx`
/// carries. The stack is the meet, so a spawn narrowing a child only ever adds
/// a layer; naming a base looser than the parent changes nothing once folded.
///
/// # Errors
/// Unknown `base_name`.
pub fn base_layer(base_name: &str, ctx: &FreezeCtx) -> Result<GrantLayer, Error> {
    let anchor = FreezeCtx::at(EnvVars::new(), ctx.cwd().clone());
    resolve_base(base_name, &anchor.with_scratch(ctx.scratch()))
}

/// A deny layer carving `paths` out of an otherwise-unrestricted fs policy.
/// A constructor, not a mutator, since composition is the stack: each caller
/// pushes the layer this returns.
///
/// Each path's walk holds the object it names, which the in-process check and
/// the OS sandbox profiles each judge, expanded to the firmlink variants.
/// A path [`host_absolute`] refuses fails the layer whole: a deny that named
/// nothing would fail open.
fn deny_layer(paths: &[PathBuf], ctx: &FreezeCtx) -> Result<GrantLayer, Error> {
    let held = paths
        .iter()
        .map(|p| held(p))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let fs = FsTable {
        deny: held
            .into_iter()
            .map(|p| Anchor::Absolute.place(p))
            .collect(),
        ..FsTable::default()
    };
    carve_out(
        Axes {
            fs: Some(fs),
            ..Axes::default()
        },
        ctx,
    )
}

/// Exclude the boot-pinned binary `own` from exec. Called only when exec is
/// restricted; refuses if `own` is `None`. An fs exclusion would refuse every
/// bundled tool.
fn self_layer(own: Option<&Path>, ctx: &FreezeCtx) -> Result<GrantLayer, Error> {
    let exe = own.ok_or_else(|| {
        Error::new(
            "exarch could not pin its own binary at startup (is it missing or unreadable?), \
             so cannot exclude it from exec",
        )
    })?;
    let exec = ExecTable {
        deny: vec![Target::File(Anchor::Absolute.place(held(exe)?))],
        ..ExecTable::default()
    };
    carve_out(
        Axes {
            exec: Some(exec),
            ..Axes::default()
        },
        ctx,
    )
}

/// The `absolute` spelling a held path freezes to.
fn held(path: &Path) -> Result<String, Error> {
    host_absolute(path).map(FrozenPath::into_string)
}

/// `t ⊕ excluded`: every request less what `excluded` denies, read as an
/// extension of the top, as `--extend` reads a fragment.
fn carve_out(excluded: Axes, ctx: &FreezeCtx) -> Result<GrantLayer, Error> {
    let fragment = decode_fragment(GrantValue::from(excluded).as_ref(), "exarch", ctx)?;
    Ok(GrantLayer::root().extend(fragment))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use ral_core::authority::grant::freeze::Place;
    use ral_core::authority::grant::value::LayerValue;
    use ral_core::authority::policy::Net;
    use ral_core::data::FOValue;

    /// The scratch spelling every test here freezes `scratch` places against:
    /// no test makes anything in it.
    fn scratch() -> String {
        std::env::temp_dir()
            .join("a-session-scratch")
            .to_string_lossy()
            .into_owned()
    }

    /// A spawned child's anchor at `cwd`, over [`scratch`].
    fn ctx_at(cwd: &Path) -> FreezeCtx {
        let cwd = ral_core::path::CwdPath::new(cwd.to_path_buf()).expect("an absolute cwd");
        FreezeCtx::at(EnvVars::new(), cwd).with_scratch(Some(scratch()))
    }

    /// The restrict file lies under the `cwd` it opens to writes, so only its
    /// own deny can refuse the write.
    #[test]
    fn an_fs_restricting_restrict_file_denies_writes_to_itself() {
        let dir = tempfile::tempdir().expect("fixture directory");
        let restrict = dir.path().join("restrict.ral");
        std::fs::write(&restrict, "return [fs: [write: [`cwd .]]]\n").expect("restrict profile");
        let Invocation { stack, .. } = for_invocation(
            &CwdPath::new(dir.path().to_path_buf()).expect("fixture cwd"),
            &scratch(),
            "dangerous",
            None,
            std::slice::from_ref(&restrict),
            None,
        )
        .unwrap();
        let runtime = ral_core::test_helper::core_runtime();
        let write = |path: &std::path::Path| {
            stack.admits_fs(
                runtime.baseline(),
                ral_core::authority::policy::FsOp::Write,
                &ral_core::test_helper::trace(path),
            )
        };
        assert!(write(&dir.path().join("sibling")), "the cwd stays writable");
        assert!(!write(&restrict));
    }

    /// `dangerous` restricts no axis, so no exclusion is pushed and no launch
    /// needs an OS sandbox: a host without bwrap still runs commands.
    #[test]
    fn a_dangerous_session_restricts_nothing() {
        let Invocation { stack, .. } = for_invocation(
            &CwdPath::new(std::env::temp_dir()).expect("temporary cwd"),
            &scratch(),
            "dangerous",
            None,
            &[],
            None,
        )
        .unwrap();
        assert!(!stack.is_restrictive(), "{stack:?}");
    }

    /// A carve-out the decoder refuses is refused in its words, hint and all.
    #[cfg(unix)]
    #[test]
    fn a_refused_carve_out_is_refused_in_the_decoders_words() {
        let ctx = ctx_at(&std::env::temp_dir());
        let why = deny_layer(&[PathBuf::from("/")], &ctx).unwrap_err();
        assert!(why.describe().contains("filesystem root"), "{why}");
        assert!(
            why.hints()
                .iter()
                .any(|h| h.starts_with("To withhold every path")),
            "{why:?}"
        );
    }

    #[test]
    fn an_unpinned_reasonable_session_refuses_to_compose() {
        let err = for_invocation(
            &CwdPath::new(std::env::temp_dir()).expect("temporary cwd"),
            &scratch(),
            "reasonable",
            None,
            &[],
            None,
        )
        .unwrap_err();
        assert_eq!(
            err.describe(),
            "exarch could not pin its own binary at startup (is it missing or unreadable?), \
             so cannot exclude it from exec"
        );
    }

    #[test]
    fn the_self_layer_excludes_only_the_supplied_binary() {
        let dir = tempfile::tempdir().expect("fixture directory");
        let own = dir.path().join("exarch");
        let sibling = dir.path().join("sibling");
        let restrict = dir.path().join("restrict.ral");
        std::fs::write(&own, "fixture").expect("fixture binary");
        std::fs::write(&sibling, "sibling").expect("sibling binary");
        std::fs::write(&restrict, "return [exec: [allow: [`dir `cwd .]]]\n")
            .expect("restrict profile");

        let Invocation { stack, .. } = for_invocation(
            &CwdPath::new(dir.path().to_path_buf()).expect("fixture cwd"),
            &scratch(),
            "dangerous",
            None,
            &[restrict],
            Some(&own),
        )
        .expect("the supplied binary can be excluded");
        let layers: Vec<&GrantLayer> = stack.layers().collect();
        let (own_layer, rest) = layers.split_last().expect("a self layer");
        for layer in rest {
            let denies = layer.axes().exec.map(|exec| exec.deny);
            assert!(denies.is_none_or(|d| d.is_empty()), "{layer:?}");
        }
        let data = |axes: Axes| FOValue::from(GrantValue::from(axes));
        let excluded = Axes {
            exec: Some(ExecTable {
                deny: vec![Target::File(Place::from(&host_absolute(&own).unwrap()))],
                ..ExecTable::default()
            }),
            ..Axes::default()
        };
        assert_eq!(
            own_layer.value(),
            LayerValue {
                grant: data(Axes::default()),
                fragment: Some(data(excluded)),
            },
            "every program, less the supplied binary"
        );

        let mut shell = ral_core::test_helper::core_shell();
        for layer in stack.layers() {
            shell.push_session_capabilities(layer.clone());
        }
        ral_core::test_access::check_file(&mut shell, &ral_core::path::shown(&own))
            .expect_err("the supplied binary is excluded");
        ral_core::test_access::check_file(&mut shell, &ral_core::path::shown(&sibling))
            .expect("the sibling remains admitted");
    }

    /// The stack ANDs the verdicts, so a `confined` layer (network off) under
    /// the looser `minimal` layer (network on) stays offline whichever order
    /// they were pushed.
    #[test]
    fn narrow_cannot_escalate_a_restricted_parent() {
        let ctx = ctx_at(&std::env::temp_dir());
        let confined = base_layer("confined", &ctx).unwrap();
        assert_eq!(
            confined.axes().net,
            Some(Net::Offline),
            "confined layer has net off"
        );
        let minimal = base_layer("minimal", &ctx).unwrap();

        let mut stack = GrantStack::root();
        stack.push(confined);
        stack.push(minimal);
        assert!(
            stack.net().is_some(),
            "naming a looser base must not turn the network back on"
        );
        assert!(
            stack.gpu().is_some(),
            "naming a looser base must not hand back the GPU"
        );
    }

    /// The bare names `profile` allows or denies.
    #[cfg(unix)]
    fn names(profile: &str) -> Vec<String> {
        let mut shell = ral_core::test_helper::core_shell();
        let value = ral_core::shell::load::evaluate_source(
            &ral_core::shell::runtime::Mooring::adrift(),
            &mut shell,
            profile,
            &ral_core::source::SourceOrigin::named("<test:names>"),
            None,
        )
        .expect("the profile evaluates");
        let value = FOValue::try_from(&value).expect("a profile is data");
        let exec = value.field("exec").expect("the profile restricts exec");
        let targets = ["allow", "deny"]
            .into_iter()
            .filter_map(|key| exec.field(key));
        let targets = targets.flat_map(|list| list.as_list().unwrap_or_default());
        targets
            .filter_map(|target| match ral_core::data::datum::untag(target) {
                Some(("name", Some(name))) => name.as_str().map(str::to_owned),
                _ => None,
            })
            .collect()
    }

    /// What the agent reads of exarch's default boot grant, pinned, on a host
    /// all fixture: cwd, scratch, home and the credential under `/fixture`,
    /// the `PATH` a stub of every name `reasonable` names.  A selector or a
    /// literal tool root names the test host's own directories, which no
    /// golden can pin: those are left out.
    #[cfg(unix)]
    #[test]
    fn the_default_boot_grant_reads_as_pinned() {
        use ral_core::protocol::probe::Grants;
        use std::os::unix::fs::PermissionsExt;
        const FIXTURE: &str = "/fixture/";
        let bin = tempfile::tempdir().expect("a stub bin");
        for name in names(include_str!("../data/reasonable.exarch.ral")) {
            let stub = bin.path().join(name);
            std::fs::write(&stub, "").expect("a stub");
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = ral_core::path::utf8(bin.path()).expect("a Unicode tempdir");
        // An empty `XDG_*_HOME` is no override: the default under `HOME`.
        let xdg = Anchor::all().filter_map(|anchor| match anchor {
            Anchor::Xdg(kind) => Some((kind.env_var(), "")),
            _ => None,
        });
        let env = [("HOME", "/fixture/home"), ("PATH", path)]
            .into_iter()
            .chain(xdg);
        let cwd = CwdPath::new(PathBuf::from("/fixture/work")).expect("an absolute cwd");
        let ctx = FreezeCtx::at(env.collect(), cwd).with_scratch(Some("/fixture/scratch"));
        let credential = PathBuf::from("/fixture/home/.local/state/exarch/credentials.json");
        let own = Path::new("/fixture/bin/exarch");
        let Invocation { stack, .. } =
            compose(&ctx, "reasonable", None, &[], &[credential], Some(own)).unwrap();
        let pinnable = |mut axes: Axes| {
            if let Some(exec) = &mut axes.exec {
                exec.allow.retain(|target| match target {
                    Target::Dir(Place {
                        anchor: Anchor::Absolute,
                        payload,
                    }) => payload.starts_with(FIXTURE),
                    _ => true,
                });
            }
            axes
        };
        let shown = stack.layers().filter(|layer| layer.is_shown());
        let layers = shown.map(|layer| LayerValue {
            grant: GrantValue::from(pinnable(layer.axes())).into(),
            ..layer.value()
        });
        crate::prompt::pin_grant_section(
            Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/policy/golden/default-boot-grant.md"
            )),
            &Grants {
                layers: layers.collect(),
                withholds_gpu: true,
            },
        );
    }
}
