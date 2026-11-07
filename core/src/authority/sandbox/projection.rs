//! The sandbox's view of authority: the meet-folded residue of a
//! [`GrantStack`] that the OS backends render.
//!
//! [`SandboxProjection::of`] folds a stack once, at spawn, because that is
//! when the OS profile is written.  The in-process guard (`crate::authority::guard`)
//! folds the same per-dimension tables afresh on every check: the fs
//! [`region`] per op, the read one carrying the [`Baseline`] and the run places,
//! and for exec the stack's [`rules`], which [`GrantStack::admit`] judges by.
//! So guard and profile cannot disagree about what the stack permits; all
//! that separates them is when the fold runs.  `detach` gates a verb rather
//! than an OS rule, so it reaches no projection.
//!
//! Support, what a launch needs beyond the policy to start a program
//! (carriers, the Linux linker), is joined into both tables here, as allows
//! that yield to every exclusion, so that no backend knows support as such.

#[cfg(unix)]
use crate::authority::policy::Extent;
use crate::authority::policy::{
    Admitted, Baseline, ExecRules, FsAllow, FsDeny, FsOp, FsRules, Gpu, GrantStack, Join,
    KernelRules, region, rules,
};
#[cfg(unix)]
use crate::authority::policy::{GrantPath, Subject, Withheld};
use crate::path::RealPath;
#[cfg(unix)]
use crate::path::{Object, Rendered, pinned_dirs, render_excluded, render_objects};
use std::collections::BTreeSet;

/// The fs half of a projection: the lists a backend renders.
///
/// Allowances and exclusions stay as the guard judges them, up to rendering:
/// an exclusion with the objects its walk held.  `A` names an allowance and
/// `X` an exclusion: [`FsAllow`] and [`FsDeny`] as folded, `Object` and
/// `Rendered` as rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsLists<A = FsAllow, X = FsDeny> {
    /// The live read region: authored reads and writes, the run places, the
    /// live support and the [`Baseline`].
    pub(crate) read: Vec<A>,
    /// The live write prefixes.  Only prefixes: a place is never written.
    pub(crate) write: Vec<A>,
    pub(crate) deny: Vec<X>,
    /// The ancestor closure of every *rendered* `deny` entry that lies within
    /// some *rendered* write name.  Both Unix backends pin each against rename
    /// and removal (Seatbelt refuses its unlink; Linux binds it onto itself),
    /// or a confined child relocates the ancestor directory itself
    /// (`mv /repo/.ssh /repo/x`, or `mv /repo /scratch/r` when the write
    /// prefix root is the ancestor) and the denied bytes resurface at a name
    /// no deny rule covers.  Derived after rendering, so both a deny's surface
    /// chain and its resolved chain are pinned.  Empty until then.
    pub(crate) pinned_dirs: Vec<X>,
}

/// Empty under any naming: rules over no paths, the shape a backend falls back
/// to at the unrestricted top.  Hand-written because the derive would demand
/// `A: Default`.
impl<A, X> Default for FsLists<A, X> {
    fn default() -> Self {
        Self {
            read: Vec::new(),
            write: Vec::new(),
            deny: Vec::new(),
            pinned_dirs: Vec::new(),
        }
    }
}

#[cfg(unix)]
impl FsLists {
    /// The read table's verdict on `s`: what the kernel reads, live
    /// support and the floor among it.  Dropping a dead allowance changes no
    /// verdict, since one supports only what an exclusion already holds.
    pub(crate) fn read_withheld(&self, s: crate::path::Subject<'_>) -> Option<Withheld<FsDeny>> {
        FsRules::new(self.read.iter().cloned(), self.deny.iter().cloned()).withheld(s)
    }
}

/// OS-renderable view of the meet-folded fs policy.
///
/// `Unrestricted` is the lattice top: the stack writes the root and excludes
/// nothing, so the profile passes fs through with broad `file-read*`/
/// `file-write*` on macOS, `--dev-bind / /` on Linux. An empty `Restricted` is
/// the other extreme: fs was attenuated to nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(any(test, feature = "test-util"), derive(Default))]
pub enum FsProjection<A = FsAllow, X = FsDeny> {
    #[cfg_attr(any(test, feature = "test-util"), default)]
    Unrestricted,
    Restricted(FsLists<A, X>),
}

impl<A, X> FsProjection<A, X> {
    /// The lists when restricted, `None` at the unrestricted top.
    pub(crate) fn lists(&self) -> Option<&FsLists<A, X>> {
        match self {
            Self::Unrestricted => None,
            Self::Restricted(r) => Some(r),
        }
    }
}

/// Exec rules for the kernel: the stack's table as the kernel holds it, its
/// support joined as allows ([`ExecRules::for_kernel`],
/// [`KernelRules::with_support`]).
///
/// `Unrestricted` where exec denotes `t`: the in-process guard alone checks,
/// and its exclusions are the table's own.  `Restricted` closes the OS layer
/// around the same table, shutting the `sh -c "PATH=…; cmd"` route by which a
/// sandboxed child re-execs binaries the guard never sees.  An empty
/// `Restricted` admits nothing.  Its paths are [`RealPath`]s, frozen at grant,
/// and stay so until the backend that needs other spellings renders them with
/// `path::render_real`, which never re-resolves one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(any(test, feature = "test-util"), derive(Default))]
pub(crate) enum ExecProjection {
    #[cfg_attr(any(test, feature = "test-util"), default)]
    Unrestricted,
    Restricted(KernelRules),
}

/// The OS-renderable projection of the effective grant, produced by
/// [`SandboxProjection::of`] after meet-folding the whole stack.
///
/// The platform backends `authority::sandbox::linux` and `authority::sandbox::macos` render it, in
/// the launching process. Unlike a [`GrantLayer`](crate::authority::policy::GrantLayer),
/// no further composition can widen it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxProjection<A = FsAllow, X = FsDeny> {
    pub fs: FsProjection<A, X>,
    pub net: bool,
    /// Whether the GPU is in reach: no layer withholds it, or this host
    /// cannot withhold it.
    pub gpu: bool,
    pub(crate) exec: ExecProjection,
}

/// A projection with every fs name rendered: what a Unix backend emits.
#[cfg(target_os = "linux")]
pub(crate) type RenderedProjection = SandboxProjection<Object, Rendered>;
#[cfg(target_os = "linux")]
pub(crate) type RenderedFs = FsProjection<Object, Rendered>;
#[cfg(target_os = "macos")]
pub(crate) type RenderedLists = FsLists<Object, Rendered>;

#[cfg(any(test, feature = "test-util"))]
impl<A, X> Default for SandboxProjection<A, X> {
    fn default() -> Self {
        Self {
            fs: FsProjection::default(),
            net: true,
            gpu: true,
            exec: ExecProjection::default(),
        }
    }
}

impl SandboxProjection {
    /// Every fs name in the projection as the names the kernel may present it
    /// by, ordering and deduping each set once on the way and deriving
    /// `pinned_dirs` from the result.  After this call no rule a backend
    /// emits can name a spelling the kernel will not present.
    ///
    /// The completeness guarantee is structural: the input is destructured
    /// exhaustively, the output constructed exhaustively, and the only way to
    /// obtain a [`Rendered`] is to render.  An allowance renders as its frozen
    /// real path, never re-resolved ([`render_objects`]); an exclusion
    /// renders as the real paths its walk held, its name's and its link's
    /// target's ([`render_excluded`]).  Exec rules pass through as real
    /// paths, for their backend to render with
    /// [`render_real`](crate::path::render_real).
    ///
    /// Pins are derived here rather than carried because they are a function
    /// of the deny and write positions: the ancestors of what a deny holds,
    /// within a write prefix ([`pinned_dirs`]).
    ///
    /// # Errors
    ///
    /// A name whose expansion cannot be spelled faithfully, as
    /// [`render_excluded`] refuses it.
    #[cfg(unix)]
    pub(crate) fn rendered(&self) -> Result<SandboxProjection<Object, Rendered>, String> {
        let Self { fs, net, gpu, exec } = self;
        let fs = match fs {
            FsProjection::Unrestricted => FsProjection::Unrestricted,
            FsProjection::Restricted(FsLists {
                read,
                write,
                deny,
                // Derived below, so nothing a caller supplied is authority.
                pinned_dirs: _,
            }) => {
                let positions: Vec<_> = (deny.iter())
                    .flat_map(|deny| deny.grant().real.positions())
                    .collect();
                let prefixes: Vec<_> = (write.iter())
                    .map(|scope| {
                        scope
                            .real()
                            .map_or_else(RealPath::universal, RealPath::clone)
                    })
                    .collect();
                FsProjection::Restricted(FsLists {
                    read: objects(read)?,
                    pinned_dirs: pinned_dirs(&positions, &prefixes)?,
                    write: objects(write)?,
                    deny: render_excluded(&positions)?,
                })
            }
        };
        Ok(SandboxProjection {
            fs,
            net: *net,
            gpu: *gpu,
            exec: exec.clone(),
        })
    }

    /// Meet-fold the stack's fs, net, gpu, and exec dimensions into the
    /// OS-renderable projection; the GPU only where this host withholds it
    /// ([`crate::authority::sandbox::WITHHOLDS_GPU`]).  Exec is the stack's
    /// table, compiled here once, as the kernel holds it.  An axis that denotes
    /// `t` projects as unrestricted, though an `every` table's run places still
    /// reach the read region.  `None` when no axis restricts, nor exec where the
    /// host's backend renders it ([`crate::authority::sandbox::EXEC_ENFORCED`]),
    /// so the caller can skip OS sandbox setup entirely.
    ///
    /// Support (carriers of `admitted`'s file, and the Linux linker where exec
    /// is restricted) is computed once for every launch with a restricted fs or
    /// exec, filtered to the live files no exec or fs exclusion covers, and
    /// joined into the read region as places and into the exec table as file
    /// allows.  A caller launching nothing passes `None`.
    pub(crate) fn of(
        grants: &GrantStack,
        baseline: &Baseline,
        admitted: Option<&Admitted>,
    ) -> Option<Self> {
        // Zipped because the two regions are `Some` on the same condition,
        // some layer held an `fs` opinion, so there is no mixed case to weigh.
        let regions = region(grants, &FsOp::Read, baseline)
            .zip(region(grants, &FsOp::Write, baseline))
            .filter(|_| !grants.fs_is_top());
        let online = grants.net_veto().is_none();
        let gpu =
            !(crate::authority::sandbox::WITHHOLDS_GPU && grants.gpu().any(|g| g == Gpu::Withheld));

        let kernel = rules(grants)
            .filter(|_| !grants.exec_is_top())
            .as_ref()
            .map(ExecRules::for_kernel);
        // Attenuated exec is worth an OS sandbox exactly where a backend
        // carries the rules into the kernel, which is the backends' fact to
        // state and `authority::sandbox::EXEC_ENFORCED`'s to answer.  The in-process guard
        // runs on every platform regardless; the kernel layer is what sees the
        // re-execs it cannot (`sh -c`).
        let exec_triggers_sandbox = crate::authority::sandbox::EXEC_ENFORCED && kernel.is_some();

        if regions.is_none() && online && gpu && !exec_triggers_sandbox {
            return None;
        }

        let live = live_support(
            kernel.as_ref(),
            regions.as_ref().map(|(read, _)| read),
            admitted,
        );
        let exec = kernel.map_or(ExecProjection::Unrestricted, |kernel| {
            ExecProjection::Restricted(kernel.with_support(&live))
        });
        let fs = match regions {
            Some((read, write)) => {
                let read = read.join(FsRules::new(
                    live.iter().map(|s| FsAllow::RunPlace(s.clone())),
                    [],
                ));
                FsProjection::Restricted(FsLists {
                    read: read.live().cloned().collect(),
                    write: write.live().cloned().collect(),
                    // Both regions carry the same denies; a disk change between
                    // the two folds can only drop a write allow, which fails
                    // closed.
                    deny: read.denies().cloned().collect(),
                    pinned_dirs: Vec::new(),
                })
            }
            None => FsProjection::Unrestricted,
        };
        Some(Self {
            fs,
            net: online,
            gpu,
            exec,
        })
    }
}

/// `scopes` as their objects, each frozen allowance by its real path.
#[cfg(unix)]
fn objects(scopes: &[FsAllow]) -> Result<Vec<Object>, String> {
    let universal = RealPath::universal();
    let none: &[RealPath] = &[];
    let named = scopes.iter().map(|scope| match scope {
        FsAllow::Prefix(GrantPath {
            written,
            real: Extent::Under { position, passed },
        }) => (position, Some(written), passed.as_slice()),
        FsAllow::Prefix(GrantPath { written, .. }) => (&universal, Some(written), none),
        FsAllow::RunPlace(place) => (place, None, none),
    });
    render_objects(named)
}

/// The live support `S'`: the carriers of the launched file and, where exec is
/// restricted, of every live file, with the Linux linkers, less every file an
/// exec exclusion (`kernel`) or an fs exclusion (`read`) covers.  Support
/// yields to every exclusion on either axis: a file that cannot be both run and
/// read serves no launch.
#[cfg(unix)]
fn live_support(
    kernel: Option<&KernelRules>,
    read: Option<&FsRules>,
    admitted: Option<&Admitted>,
) -> BTreeSet<RealPath> {
    use crate::authority::policy::Program;
    let launched = admitted.and_then(|a| match a.program() {
        Program::File { located, .. } => Some(located.real()),
        Program::Tool(_) => None,
    });
    let files = kernel.into_iter().flat_map(KernelRules::allowed_files);
    let excluded = |trace: &crate::path::Trace| {
        let at = crate::path::Subject::Access(trace);
        kernel.is_some_and(|k| k.excludes(Subject::Program(at)))
            || read.is_some_and(|r| r.excludes(at))
    };
    let mut support = super::carriers(launched.into_iter().chain(files), excluded);
    if kernel.is_some() {
        support.extend(super::carriers::linkers(excluded));
    }
    support
}

/// No kernel exec layer here ([`crate::authority::sandbox::EXEC_ENFORCED`]), so nothing
/// to carry.
#[cfg(windows)]
fn live_support(
    _: Option<&KernelRules>,
    _: Option<&FsRules>,
    _: Option<&Admitted>,
) -> BTreeSet<RealPath> {
    BTreeSet::new()
}

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod rendered_tests {
    use super::*;
    use crate::path::{Excluded, FrozenPath, Trace, utf8};

    fn prefix(path: &str) -> FsAllow {
        FsAllow::Prefix(
            GrantPath::<Trace>::walk(FrozenPath::from_surface(path)).expect("a prefix walks"),
        )
    }

    fn excluding(path: &str) -> FsDeny {
        FsDeny(
            GrantPath::<Excluded>::walk(FrozenPath::from_surface(path))
                .expect("an exclusion walks"),
        )
    }

    fn projection(write: &[&str], deny: &[&str], pinned: &[&str]) -> SandboxProjection {
        SandboxProjection {
            fs: FsProjection::Restricted(FsLists {
                read: Vec::new(),
                write: write.iter().copied().map(prefix).collect(),
                deny: deny.iter().copied().map(excluding).collect(),
                pinned_dirs: pinned.iter().copied().map(excluding).collect(),
            }),
            net: true,
            gpu: true,
            exec: ExecProjection::default(),
        }
    }

    /// The pins the traversal derives, through the real renderer.
    /// Containment, not equality: the renderer may add spellings.
    fn pinned(write: &[&str], deny: &[&str], carried: &[&str]) -> Vec<String> {
        let out = projection(write, deny, carried)
            .rendered()
            .expect("ASCII paths render");
        let lists = out.fs.lists().expect("restricted in, restricted out");
        lists
            .pinned_dirs
            .iter()
            .map(|r| r.as_str().to_string())
            .collect()
    }

    /// The write prefix root is itself a proper ancestor of a deep-enough
    /// deny, so it is pinned alongside the two intermediate directories.
    #[test]
    fn pins_every_ancestor_within_the_write_prefix_including_its_root() {
        let dirs = pinned(&["/repo"], &["/repo/a/b/secret"], &[]);
        for expect in ["/repo", "/repo/a", "/repo/a/b"] {
            assert!(dirs.iter().any(|d| d == expect), "got {dirs:?}");
        }
    }

    #[test]
    fn pins_nothing_when_no_write_prefix_covers_the_denys_chain() {
        assert_eq!(
            pinned(&[], &["/repo/.ssh/id_rsa"], &[]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn pins_nothing_for_a_deny_outside_every_prefix() {
        assert_eq!(
            pinned(&["/repo"], &["/etc/secret"], &[]),
            Vec::<String>::new()
        );
    }

    /// A supplied pin is not authority: the traversal recomputes the set.
    #[test]
    fn a_carried_pin_is_replaced_by_the_derived_one() {
        let dirs = pinned(&["/repo"], &["/repo/a/secret"], &["/somewhere/else"]);
        assert!(dirs.iter().any(|d| d == "/repo/a"), "got {dirs:?}");
        assert!(
            !dirs.iter().any(|d| d.contains("somewhere")),
            "got {dirs:?}"
        );
    }

    /// An allowance renders as its frozen real path, and a prefix granted
    /// through a symlink never as the surface it was written by on macOS.
    #[test]
    fn an_allowance_renders_as_its_frozen_real_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::test_helper::real(tmp.path());
        let real = root.as_path().join("real");
        let link = root.as_path().join("link");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let linked = FsAllow::Prefix(
            GrantPath::<Trace>::walk(FrozenPath::from_surface(utf8(&link).unwrap()))
                .expect("a link walks"),
        );
        let projection = SandboxProjection {
            fs: FsProjection::Restricted(FsLists {
                read: vec![linked],
                ..FsLists::default()
            }),
            net: true,
            gpu: true,
            exec: ExecProjection::default(),
        };
        let out = projection.rendered().expect("ASCII paths render");
        let lists = out.fs.lists().expect("restricted");
        let object = &lists.read[0];
        assert_eq!(object.real.as_str(), utf8(&real).unwrap());
        #[cfg(target_os = "macos")]
        assert!(object.names().all(|n| !n.as_str().contains("link")));
        #[cfg(target_os = "linux")]
        assert_eq!(object.spellings.len(), 1);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{ExecProjection, FsProjection};
    use crate::authority::policy::{Baseline, Capabilities, ExecGrant, ExecKey, FsPolicy, Gpu};
    use crate::path::{FrozenPath, utf8};
    use crate::test_access::fs_read_paths;
    use crate::test_helper::{layer, stack};

    fn authored(read: Option<&str>, run: Option<&str>) -> Capabilities {
        let prefix = FrozenPath::from_surface;
        Capabilities {
            fs: read.map(|w| FsPolicy {
                read_prefixes: vec![prefix(w)],
                write_prefixes: Vec::new(),
                deny_paths: Vec::new(),
            }),
            exec: run.map(|dir| ExecGrant::of([ExecKey::Dir(prefix(dir))], [])),
            ..Capabilities::default()
        }
    }

    fn project(base: Capabilities) -> super::SandboxProjection {
        project_layer(layer(base))
    }

    fn project_layer(base: crate::authority::policy::GrantLayer) -> super::SandboxProjection {
        crate::test_helper::core_shell()
            .with_layer(base, |sh| sh.sandbox_projection().expect("restricted"))
    }

    /// The run place joins the read region, and nothing becomes writable.
    #[test]
    fn a_run_place_is_read_and_never_written() {
        let projection = project(authored(Some("/ral-test/w"), Some("/ral-test/bin")));
        let read = fs_read_paths(&projection.fs).expect("fs restricted");
        assert!(read.iter().any(|p| p == "/ral-test/bin"), "{read:?}");
        let lists = projection.fs.lists().expect("fs restricted");
        assert_eq!(lists.write, Vec::new());
    }

    /// The projection's read lists give the read table's verdict and witness,
    /// its dead allowances dropped.
    #[test]
    fn the_read_lists_judge_as_the_read_table() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = crate::test_helper::Tree::lay(tmp.path(), &["/w/a/f", "/w/secret/f", "/x/f"]);
        let at = |p: &str| FrozenPath::from_surface(&tree.at(p));
        let grants = stack([layer(Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec![at("/w"), at("/w/secret/f")],
                write_prefixes: Vec::new(),
                deny_paths: vec![at("/w/secret")],
            }),
            ..Capabilities::default()
        })]);
        let baseline = Baseline::default();
        let projection = super::SandboxProjection::of(&grants, &baseline, None).unwrap();
        let lists = projection.fs.lists().expect("fs restricted");
        let table = super::region(&grants, &super::FsOp::Read, &baseline).unwrap();
        assert!(
            table.live().count() < table.allows().count(),
            "a dead allowance"
        );
        for p in ["/w/a/f", "/w/secret/f", "/w/secret", "/x/f"] {
            let access = crate::test_helper::trace(tree.path(p));
            let s = crate::path::Subject::Access(&access);
            assert_eq!(lists.read_withheld(s), table.withheld(s), "{p}");
        }
    }

    /// Exec that denotes `t` projects as unrestricted, though its places
    /// still reach the read region.
    #[test]
    fn exec_that_denotes_top_is_unrestricted_and_its_places_still_read() {
        let every = layer(authored(Some("/ral-test/w"), Some("/ral-test/bin"))).with_every();
        let projection = project_layer(every);
        assert_eq!(projection.exec, ExecProjection::Unrestricted);
        let read = fs_read_paths(&projection.fs).expect("fs restricted");
        assert!(read.iter().any(|p| p == "/ral-test/bin"), "{read:?}");
    }

    /// A withheld GPU alone engages the sandbox, as `offline` does.
    #[test]
    fn a_withheld_gpu_alone_engages_the_sandbox() {
        let withheld = project(Capabilities {
            gpu: Some(Gpu::Withheld),
            ..Capabilities::default()
        });
        assert!(!withheld.gpu && withheld.net);
        assert_eq!(withheld.fs, FsProjection::Unrestricted);
        assert_eq!(withheld.exec, ExecProjection::Unrestricted);
        assert!(project(authored(Some("/ral-test/w"), None)).gpu);
    }

    /// Support reads a system script's interpreter where the fs grant names
    /// neither, with exec omitted or restricted to the script.
    #[cfg(unix)]
    #[test]
    fn live_support_reads_a_system_scripts_interpreter() {
        let Some((script, interp)) = crate::authority::sandbox::carriers::system_script() else {
            eprintln!("skipping: this host has no system script");
            return;
        };
        let only_script = ExecGrant::of(
            [ExecKey::Path(
                FrozenPath::freeze(script.as_path()).expect("a Unicode path"),
            )],
            [],
        );
        for exec in [None, Some(only_script)] {
            let stack = stack([layer(Capabilities {
                exec,
                ..authored(Some("/ral-test/w"), None)
            })]);
            let baseline = Baseline::of_host().expect("the host baseline resolves");
            let program =
                crate::authority::policy::Program::file(script.as_path().into()).expect("a script");
            let admitted = stack.admit(&baseline, program).expect("admitted");
            let rendered = super::SandboxProjection::of(&stack, &baseline, Some(&admitted))
                .expect("fs restricted")
                .rendered()
                .expect("system paths render");
            let read = &rendered.fs.lists().expect("fs restricted").read;
            let carried = read
                .iter()
                .any(|o| o.real.as_str() == utf8(interp.as_path()).unwrap());
            assert!(carried, "{interp} is not read: {read:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_excluded_system_seed_contributes_no_interpreter_on_either_axis() {
        let Some((script, interpreter)) = super::super::carriers::system_script() else {
            eprintln!("skipping: this host has no system script");
            return;
        };
        let baseline = Baseline::of_host().unwrap();
        let admitted = super::super::launch::admitted(
            crate::authority::policy::Program::file(script.as_path().into()).unwrap(),
        );
        assert!(super::live_support(None, None, Some(&admitted)).contains(&interpreter));
        for by_exec in [true, false] {
            let written = FrozenPath::freeze(script.as_path()).unwrap();
            let caps = if by_exec {
                Capabilities {
                    exec: Some(ExecGrant::of([], [ExecKey::Path(written)])),
                    ..Capabilities::default()
                }
            } else {
                Capabilities {
                    fs: Some(crate::authority::policy::FsPolicy {
                        deny_paths: vec![written],
                        ..Default::default()
                    }),
                    ..Capabilities::default()
                }
            };
            let grants = stack([layer(caps)]);
            let kernel = super::rules(&grants).map(|exec| exec.for_kernel());
            let read = super::region(&grants, &super::FsOp::Read, &baseline);
            let support = super::live_support(kernel.as_ref(), read.as_ref(), Some(&admitted));
            assert!(
                !support.contains(&interpreter),
                "excluded seed contributed {interpreter}: {support:?}"
            );
        }
    }
}

#[cfg(all(test, windows))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod windows_tests {
    /// Windows cannot withhold the GPU, so a frame that asks engages no sandbox
    /// and refuses nothing.
    #[test]
    fn a_withheld_gpu_engages_nothing_on_windows() {
        let layer = crate::test_helper::layer(crate::authority::policy::Capabilities {
            gpu: Some(crate::authority::policy::Gpu::Withheld),
            ..Default::default()
        });
        let projection =
            crate::test_helper::core_shell().with_layer(layer, |sh| sh.sandbox_projection());
        assert_eq!(projection, None);
    }
}
