//! The sandbox's view of authority: the meet-folded residue of a
//! [`GrantStack`] that the OS backends render.
//!
//! [`SandboxProjection::of`] folds a stack once, at spawn, because that is
//! when the OS profile is written.  The in-process guard (`crate::guard`)
//! folds the same per-dimension tables afresh on every check: the fs
//! [`region`] per op, the read one carrying the [`Floor`] and the run places,
//! and for exec the very table [`GrantStack::admit`] judged with.  So guard
//! and profile cannot disagree about what the stack permits; all that
//! separates them is when the fold runs.  `detach` gates a verb rather than an
//! OS rule, so it reaches no projection.
//!
//! Support, what a launch needs beyond the policy to start a program
//! (carriers, the Linux linker), is joined into both tables here, as allows
//! that yield to every exclusion, so that no backend knows support as such.

#[cfg(unix)]
use crate::capability::Extent;
use crate::capability::GrantPath;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::capability::Subject;
use crate::capability::{
    Admitted, ExecRules, Floor, FsAllow, FsOp, FsRules as FsTable, GrantStack, Join, Net, region,
    rules, running_region,
};
use crate::path::{Excluded, RealPath};
#[cfg(unix)]
use crate::path::{Object, Rendered, pinned_dirs, render_excluded, render_objects};
use std::collections::BTreeSet;

/// The fs half of a projection.
///
/// Allowances and exclusions stay as the guard judges them, up to rendering:
/// an exclusion with the objects its walk held.  `A` names an allowance and
/// `X` an exclusion: [`FsAllow`] and [`GrantPath<Excluded>`] as folded,
/// [`Object`] and [`Rendered`] as rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsRules<A = FsAllow, X = GrantPath<Excluded>> {
    /// The live read region: authored reads and writes, the run places, the
    /// live support and the [`Floor`].
    pub(crate) read: Vec<A>,
    /// The live write prefixes.  Only prefixes: a place is never written.
    pub(crate) write: Vec<A>,
    pub(crate) deny: Vec<X>,
    /// The ancestor closure of every *rendered* `deny` entry that lies within
    /// some *rendered* write name.  The macOS backend pins each against rename
    /// and unlink, or a confined child relocates the ancestor directory itself
    /// (`mv /repo/.ssh /repo/x`, or `mv /repo /scratch/r` when the write
    /// prefix root is the ancestor) and the denied bytes resurface at a name
    /// no deny rule covers.  Derived after rendering, so both a deny's surface
    /// chain and its resolved chain are pinned.  Empty until then.
    pub(crate) pinned_dirs: Vec<X>,
}

/// Empty under any naming: rules over no paths, the shape a backend falls back
/// to at the unrestricted top.  Hand-written because the derive would demand
/// `A: Default`.
impl<A, X> Default for FsRules<A, X> {
    fn default() -> Self {
        Self {
            read: Vec::new(),
            write: Vec::new(),
            deny: Vec::new(),
            pinned_dirs: Vec::new(),
        }
    }
}

/// OS-renderable view of the meet-folded fs policy.
///
/// `Unrestricted` is the lattice top: the stack writes the root and excludes
/// nothing, so the profile passes fs through with broad `file-read*`/
/// `file-write*` on macOS, `--dev-bind / /` on Linux. An empty `Restricted` is
/// the other extreme: fs was attenuated to nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FsProjection<A = FsAllow, X = GrantPath<Excluded>> {
    #[default]
    Unrestricted,
    Restricted(FsRules<A, X>),
}

impl<A, X> FsProjection<A, X> {
    /// The rules when restricted, `None` at the unrestricted top.
    pub(crate) fn rules(&self) -> Option<&FsRules<A, X>> {
        match self {
            Self::Unrestricted => None,
            Self::Restricted(r) => Some(r),
        }
    }
}

/// Exec rules for the kernel: the table the in-process guard judged with, its
/// support joined as allows ([`ExecRules::for_kernel`]).
///
/// `Unrestricted` where exec denotes `t`: the in-process guard alone checks,
/// and its exclusions are the table's own.  `Restricted` closes the OS layer
/// around the same table, shutting the `sh -c "PATH=…; cmd"` route by which a
/// sandboxed child re-execs binaries the guard never sees.  An empty
/// `Restricted` admits nothing.  Its paths are
/// [`RealPath`](crate::path::RealPath)s, frozen at grant, and stay so until
/// the backend that needs other spellings renders them with
/// [`render_real`](crate::path::render_real), which never re-resolves one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum ExecProjection {
    #[default]
    Unrestricted,
    Restricted(ExecRules),
}

/// The OS-renderable projection of the effective grant, produced by
/// [`SandboxProjection::of`] after meet-folding the whole stack.
///
/// The platform backends `sandbox::linux` and `sandbox::macos` render it, in
/// the launching process. Unlike a [`Capabilities`](crate::capability::Capabilities)
/// frame, no further composition can widen it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxProjection<A = FsAllow, X = GrantPath<Excluded>> {
    pub fs: FsProjection<A, X>,
    pub net: bool,
    pub(crate) exec: ExecProjection,
}

/// A projection with every fs name rendered: what a Unix backend emits.
#[cfg(target_os = "linux")]
pub(crate) type RenderedProjection = SandboxProjection<Object, Rendered>;
#[cfg(target_os = "linux")]
pub(crate) type RenderedFs = FsProjection<Object, Rendered>;
#[cfg(target_os = "macos")]
pub(crate) type RenderedRules = FsRules<Object, Rendered>;

impl<A, X> Default for SandboxProjection<A, X> {
    fn default() -> Self {
        Self {
            fs: FsProjection::default(),
            net: true,
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
        let Self { fs, net, exec } = self;
        let fs = match fs {
            FsProjection::Unrestricted => FsProjection::Unrestricted,
            FsProjection::Restricted(FsRules {
                read,
                write,
                deny,
                // Derived below, so nothing a caller supplied is authority.
                pinned_dirs: _,
            }) => {
                let holds = || deny.iter().map(|deny| &deny.real);
                let prefixes: Vec<_> = (write.iter())
                    .map(|scope| {
                        scope
                            .real()
                            .map_or_else(RealPath::universal, RealPath::clone)
                    })
                    .collect();
                FsProjection::Restricted(FsRules {
                    read: objects(read)?,
                    pinned_dirs: pinned_dirs(holds(), &prefixes)?,
                    write: objects(write)?,
                    deny: render_excluded(holds())?,
                })
            }
        };
        Ok(SandboxProjection {
            fs,
            net: *net,
            exec: exec.clone(),
        })
    }

    /// Meet-fold the stack's fs, net, and exec dimensions into the
    /// OS-renderable projection.  Exec is projected from the table that
    /// admitted `admitted`; a caller launching nothing passes `None`, and the
    /// stack is compiled afresh.  An axis that denotes `t` projects as
    /// unrestricted, though an `every` table's run places still reach the read
    /// region.  `None` when no axis restricts, nor exec where the host's
    /// backend renders it ([`crate::sandbox::EXEC_ENFORCED`]), so the caller
    /// can skip OS sandbox setup entirely.
    ///
    /// Support (carriers, and the Linux linker where exec is restricted) is
    /// computed once for every launch with a restricted fs or exec, filtered to
    /// the live files no exec or fs exclusion covers, and joined into the read
    /// region as places and into the exec table as file allows.
    pub(crate) fn of(
        grants: &GrantStack,
        floor: &Floor,
        admitted: Option<&Admitted>,
    ) -> Option<Self> {
        // Zipped because the two regions are `Some` on the same condition,
        // some layer held an `fs` opinion, so there is no mixed case to weigh.
        let regions = running_region(grants, floor)
            .zip(region(grants, &FsOp::Write, floor))
            .filter(|_| !grants.fs_is_top());
        let online = !grants.net().any(|n| n == Net::Offline);

        let compiled = admitted.is_none().then(|| rules(grants)).flatten();
        let table = admitted
            .map_or(compiled.as_ref(), Admitted::rules)
            .filter(|_| !grants.exec_is_top());
        // Attenuated exec is worth an OS sandbox exactly where a backend
        // carries the rules into the kernel, which is the backends' fact to
        // state and `sandbox::EXEC_ENFORCED`'s to answer.  The in-process guard
        // runs on every platform regardless; the kernel layer is what sees the
        // re-execs it cannot (`sh -c`).
        let exec_triggers_sandbox = crate::sandbox::EXEC_ENFORCED && table.is_some();

        if regions.is_none() && online && !exec_triggers_sandbox {
            return None;
        }

        let live = live_support(table, regions.as_ref().map(|(read, _)| read), admitted);
        let exec = table.map_or(ExecProjection::Unrestricted, |table| {
            ExecProjection::Restricted(table.for_kernel(&live))
        });
        let fs = match regions {
            Some((read, write)) => {
                let read = read.join(FsTable::new(
                    live.iter().map(|s| FsAllow::RunPlace(s.clone())),
                    [],
                ));
                FsProjection::Restricted(FsRules {
                    read: read.live().cloned().collect(),
                    write: write.live().cloned().collect(),
                    // Both regions carry the same denies; a disk change between
                    // the two folds can only drop a write allow, which fails
                    // closed.
                    deny: read.denies().map(|deny| deny.grant().clone()).collect(),
                    pinned_dirs: Vec::new(),
                })
            }
            None => FsProjection::Unrestricted,
        };
        Some(Self {
            fs,
            net: online,
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
/// exec exclusion (`table`) or an fs exclusion (`read`) covers.  Support
/// yields to every exclusion on either axis: a file that cannot be both run and
/// read serves no launch.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn live_support(
    table: Option<&ExecRules>,
    read: Option<&FsTable>,
    admitted: Option<&Admitted>,
) -> BTreeSet<RealPath> {
    use crate::capability::Program;
    let launched = admitted.and_then(|a| match a.program() {
        Program::File { located, .. } => Some(located.real()),
        Program::Tool(_) => None,
    });
    let files = table.into_iter().flat_map(ExecRules::allowed_files);
    let mut support = super::carriers(launched.into_iter().chain(files));
    if table.is_some() {
        support.extend(super::carriers::linkers());
    }
    support.retain(|s| {
        let at = crate::path::Subject::Position(s);
        !table.is_some_and(|t| t.excludes(Subject::Program(at)))
            && !read.is_some_and(|r| r.excludes(at))
    });
    support
}

/// No kernel exec layer here ([`crate::sandbox::EXEC_ENFORCED`]), so nothing
/// to carry.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn live_support(
    _: Option<&ExecRules>,
    _: Option<&FsTable>,
    _: Option<&Admitted>,
) -> BTreeSet<RealPath> {
    BTreeSet::new()
}

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod rendered_tests {
    use super::*;
    use crate::path::{FrozenPath, Trace, utf8};

    fn prefix(path: &str) -> FsAllow {
        FsAllow::Prefix(
            GrantPath::<Trace>::walk(FrozenPath::from_surface(path)).expect("a prefix walks"),
        )
    }

    fn excluding(path: &str) -> GrantPath<Excluded> {
        GrantPath::<Excluded>::walk(FrozenPath::from_surface(path)).expect("an exclusion walks")
    }

    fn projection(write: &[&str], deny: &[&str], pinned: &[&str]) -> SandboxProjection {
        SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                read: Vec::new(),
                write: write.iter().copied().map(prefix).collect(),
                deny: deny.iter().copied().map(excluding).collect(),
                pinned_dirs: pinned.iter().copied().map(excluding).collect(),
            }),
            net: true,
            exec: ExecProjection::default(),
        }
    }

    /// The pins the traversal derives, through the real renderer.
    /// Containment, not equality: the renderer may add spellings.
    fn pinned(write: &[&str], deny: &[&str], carried: &[&str]) -> Vec<String> {
        let out = projection(write, deny, carried)
            .rendered()
            .expect("ASCII paths render");
        let rules = out.fs.rules().expect("restricted in, restricted out");
        rules
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
            fs: FsProjection::Restricted(FsRules {
                read: vec![linked],
                ..FsRules::default()
            }),
            net: true,
            exec: ExecProjection::default(),
        };
        let out = projection.rendered().expect("ASCII paths render");
        let rules = out.fs.rules().expect("restricted");
        let object = &rules.read[0];
        assert_eq!(object.real.as_str(), utf8(&real).unwrap());
        #[cfg(target_os = "macos")]
        assert!(object.names().all(|n| !n.as_str().contains("link")));
        #[cfg(target_os = "linux")]
        assert_eq!(object.spellings.len(), 1);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::ExecProjection;
    use crate::capability::{Capabilities, Evidence, ExecGrant, ExecKey, Floor, FsPolicy};
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
            exec: run
                .map(|dir| std::iter::once((ExecKey::Dir(prefix(dir)), Evidence::ALLOW)).collect()),
            ..Capabilities::default()
        }
    }

    fn project(base: Capabilities) -> super::SandboxProjection {
        crate::test_helper::core_shell().with_layer(layer(base), |sh| {
            sh.sandbox_projection().expect("restricted")
        })
    }

    /// The run place joins the read region, and nothing becomes writable.
    #[test]
    fn a_run_place_is_read_and_never_written() {
        let projection = project(authored(Some("/ral-test/w"), Some("/ral-test/bin")));
        let read = fs_read_paths(&projection.fs).expect("fs restricted");
        assert!(read.iter().any(|p| p == "/ral-test/bin"), "{read:?}");
        let rules = projection.fs.rules().expect("fs restricted");
        assert_eq!(rules.write, Vec::new());
    }

    /// Exec that denotes `t` projects as unrestricted, though its places
    /// still reach the read region.
    #[test]
    fn exec_that_denotes_top_is_unrestricted_and_its_places_still_read() {
        let every = std::iter::once((
            ExecKey::Dir(FrozenPath::from_surface("/ral-test/bin")),
            Evidence::ALLOW,
        ))
        .collect::<ExecGrant>()
        .with_every();
        let projection = project(Capabilities {
            exec: Some(every),
            ..authored(Some("/ral-test/w"), None)
        });
        assert_eq!(projection.exec, ExecProjection::Unrestricted);
        let read = fs_read_paths(&projection.fs).expect("fs restricted");
        assert!(read.iter().any(|p| p == "/ral-test/bin"), "{read:?}");
    }

    /// Support reads a system script's interpreter where the fs grant names
    /// neither, with exec omitted or restricted to the script.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn live_support_reads_a_system_scripts_interpreter() {
        let Some((script, interp)) = crate::sandbox::carriers::system_script() else {
            eprintln!("skipping: this host has no system script");
            return;
        };
        let only_script = [(
            ExecKey::Path(FrozenPath::freeze(script.as_path()).expect("a Unicode path")),
            Evidence::ALLOW,
        )];
        for exec in [None, Some(only_script.into_iter().collect())] {
            let stack = stack([layer(Capabilities {
                exec,
                ..authored(Some("/ral-test/w"), None)
            })]);
            let floor = Floor::of_host().expect("the host floor resolves");
            let program =
                crate::capability::Program::file(script.as_path().into()).expect("a script");
            let admitted = stack.admit(&floor, program, Vec::new()).expect("admitted");
            let rendered = super::SandboxProjection::of(&stack, &floor, Some(&admitted))
                .expect("fs restricted")
                .rendered()
                .expect("system paths render");
            let read = &rendered.fs.rules().expect("fs restricted").read;
            let carried = read
                .iter()
                .any(|o| o.real.as_str() == utf8(interp.as_path()).unwrap());
            assert!(carried, "{interp} is not read: {read:?}");
        }
    }
}
