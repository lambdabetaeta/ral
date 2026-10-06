//! The sandbox's view of authority: the meet-folded residue of a
//! [`GrantStack`] that the OS backends render.
//!
//! [`SandboxProjection::of`] folds a stack once, at spawn, because that is
//! when the OS profile is written.  The in-process guard (`crate::guard`)
//! folds the same per-dimension tables afresh on every check: the fs
//! [`region`] per op, and for exec the very table [`GrantStack::admit`]
//! judged with.  So guard and profile cannot disagree about what the stack
//! permits; all that separates them is when the fold runs.  `detach` gates a
//! verb rather than an OS rule, so it reaches no projection.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::capability::Program;
use crate::capability::{Admitted, ExecRules, FsOp, GrantStack, region, rules};
use crate::path::{FrozenPath, Rendered, Resolver, render_paths, rendered_pins};
#[cfg(unix)]
use std::borrow::Borrow;
use std::collections::BTreeSet;

/// The fs half of a projection, its paths named in `N`.
///
/// Not [`FsPolicy`]: that is the grant-layer lattice element, whose
/// [`FrozenPath`]es carry the `resolved` form the meet keys on.  Nothing
/// below the fold reads it, so the projection holds plain surface spellings
/// and each backend widens them into its own name class at render time,
/// which is exactly what `N` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsRules<N> {
    pub read_prefixes: Vec<N>,
    pub write_prefixes: Vec<N>,
    pub deny_paths: Vec<N>,
    /// The ancestor closure of every *rendered* `deny_paths` entry that lies
    /// within some *rendered* write name.  The macOS backend pins each
    /// against rename and unlink, or a confined child relocates the ancestor
    /// directory itself (`mv /repo/.ssh /repo/x`, or `mv /repo /scratch/r`
    /// when the write prefix root is the ancestor) and the denied bytes
    /// resurface at a name no deny rule covers.  Rendering runs before the
    /// ancestor walk (`SandboxProjection::rendered`), so both a deny's
    /// surface chain and its resolved chain are pinned, and an alias
    /// `W/alias → W/top/deep` with deny `W/alias/secret` pins `W/top` too —
    /// an ancestor chain only the resolved spelling reveals.
    pub(crate) pinned_dirs: Vec<N>,
}

/// Empty under any naming: rules over no paths, the shape a backend falls back
/// to at the unrestricted top.  Hand-written because the derive would demand
/// `N: Default`, which a name minted only by expansion cannot meet.
impl<N> Default for FsRules<N> {
    fn default() -> Self {
        Self {
            read_prefixes: Vec::new(),
            write_prefixes: Vec::new(),
            deny_paths: Vec::new(),
            pinned_dirs: Vec::new(),
        }
    }
}

/// How the write region stands to one admitted exec directory.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteReach {
    /// A write prefix that reaches it lies within an admit that holds it: the
    /// writes were named at or below an admit at or above this one, so
    /// authoring binaries here is the grant's intent.
    Trusted,
    /// A write prefix covers it and none was named so, so the child could
    /// author binaries the admit runs without the grant having said so.
    Covered,
    Apart,
}

#[cfg(unix)]
impl FsRules<Rendered> {
    /// The reach of the write prefixes over `admit`, itself among `admitted`.
    /// Trusted when some write prefix `w` that covers `admit` or lies inside it
    /// lies within (or is) an admitted dir that holds `admit`; covered when
    /// some `w` covers it and none qualifies; apart otherwise.  Everything is
    /// rendered names, so the comparison is the plain prefix test on spellings.
    pub(crate) fn write_reach(
        &self,
        admit: &Rendered,
        admitted: &[impl Borrow<Rendered>],
    ) -> WriteReach {
        let writes = &self.write_prefixes;
        let named = writes.iter().any(|w| {
            (w.holds(admit.as_str()) || admit.holds(w.as_str()))
                && admitted.iter().any(|t| {
                    let t = t.borrow();
                    t.holds(admit.as_str()) && t.holds(w.as_str())
                })
        });
        if named {
            WriteReach::Trusted
        } else if writes.iter().any(|w| w.holds(admit.as_str())) {
            WriteReach::Covered
        } else {
            WriteReach::Apart
        }
    }
}

/// OS-renderable view of the meet-folded fs policy.
///
/// `Unrestricted` is the lattice top — no layer attenuated fs, so the profile
/// passes it through with broad `file-read*`/`file-write*` on macOS,
/// `--dev-bind / /` on Linux. An empty `Restricted` is the other extreme: fs
/// was attenuated to nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsProjection<N = String> {
    Unrestricted,
    Restricted(FsRules<N>),
}

/// Hand-written so the top is reachable for any naming, `Rendered` included:
/// the derive would demand `N: Default`, which a name minted only by
/// expansion cannot satisfy.
impl<N> Default for FsProjection<N> {
    fn default() -> Self {
        Self::Unrestricted
    }
}

impl<N> FsProjection<N> {
    /// The rules when restricted, `None` at the unrestricted top.  Renderers
    /// wanting only the prefixes match on this; the macOS profile builder
    /// branches on the variant, since the two emit different SBPL shapes.
    pub(crate) fn rules(&self) -> Option<&FsRules<N>> {
        match self {
            Self::Unrestricted => None,
            Self::Restricted(r) => Some(r),
        }
    }
}

/// Exec rules for the kernel: the table the in-process guard judged with, its
/// carriers admitted and each file at its final verdict
/// ([`ExecRules::for_kernel`]).
///
/// Under `Unrestricted` the in-process guard is the only check; `Restricted`
/// closes the OS layer around the same table, shutting the `sh -c
/// "PATH=…; cmd"` route by which a sandboxed child re-execs binaries the
/// guard never sees.  An empty `Restricted` admits nothing.  Its paths are
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
///
/// `N` is how the projection *names* the objects its fs rules rule over;
/// exec rules name real paths throughout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxProjection<N = String> {
    pub fs: FsProjection<N>,
    pub net: bool,
    pub(crate) exec: ExecProjection,
}

impl<N> Default for SandboxProjection<N> {
    fn default() -> Self {
        Self {
            fs: FsProjection::default(),
            net: true,
            exec: ExecProjection::default(),
        }
    }
}

impl SandboxProjection<String> {
    /// Every fs path in the projection as the names the kernel may present it
    /// by, ordering and deduping each fs set once on the way and deriving
    /// `pinned_dirs` from the result.  After this call no rule a backend
    /// emits can name a spelling the kernel will not present.
    ///
    /// The completeness guarantee is structural, not promised: the input is
    /// destructured exhaustively, the output constructed exhaustively, and the
    /// only way to obtain a `Vec<Rendered>` is to render.  Exec rules pass
    /// through as real paths, for their backend to render with
    /// [`render_real`](crate::path::render_real).  A path set added later
    /// therefore fails to compile until it too is threaded — which is the point, since every under-enforcement of
    /// this class has been someone forgetting to expand one new list.
    ///
    /// Pins are derived here rather than carried because they are a *function*
    /// of the deny paths and the write prefixes — but only once both are
    /// rendered: containment before expansion and after do not commute, since
    /// a name reached only through a symlinked chain has ancestors the
    /// surface spelling never mentions.  So the deny and write sets render
    /// first, and the ancestor walk runs on their rendered output.
    ///
    /// # Errors
    ///
    /// A name whose expansion cannot be spelled faithfully, as
    /// [`render_paths`] refuses it.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn rendered(&self) -> Result<SandboxProjection<Rendered>, String> {
        let Self { fs, net, exec } = self;
        let ordered = |ps: &[String]| -> Vec<String> {
            ps.iter()
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        };
        let fs = match fs {
            FsProjection::Unrestricted => FsProjection::Unrestricted,
            FsProjection::Restricted(FsRules {
                read_prefixes,
                write_prefixes,
                deny_paths,
                // Derived below, so nothing a caller or the wire supplied is
                // authority — see [`FsRules::pinned_dirs`].
                pinned_dirs: _,
            }) => {
                let write_prefixes = render_paths(&ordered(write_prefixes))?;
                let deny_paths = render_paths(&ordered(deny_paths))?;
                let pinned_dirs = rendered_pins(&deny_paths, &write_prefixes);
                FsProjection::Restricted(FsRules {
                    read_prefixes: render_paths(&ordered(read_prefixes))?,
                    write_prefixes,
                    deny_paths,
                    pinned_dirs,
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
    /// admitted `admitted`, with the carriers of its program; a caller
    /// launching nothing passes `None`, and the stack is compiled afresh.
    /// `None` when no layer restricts fs or net, nor exec where the host's
    /// backend renders it ([`crate::sandbox::EXEC_ENFORCED`]), so the caller
    /// can skip OS sandbox setup entirely.
    pub(crate) fn of(
        grants: &GrantStack,
        resolver: &Resolver,
        admitted: Option<&Admitted>,
    ) -> Option<Self> {
        // Traced because this fold is not the pure reduction it reads as:
        // every re-freeze canonicalises against the filesystem, and compiling
        // the exec table walks the host `PATH` for every bare key, so its cost
        // tracks the host's fs latency and is paid again on each rebuild.
        #[cfg(debug_assertions)]
        let t_fold = std::time::Instant::now();
        // Zipped because the two regions are `Some` on the same condition,
        // some layer held an `fs` opinion, so there is no mixed case to weigh.
        let regions =
            region(grants, resolver, &FsOp::Read).zip(region(grants, resolver, &FsOp::Write));
        let net = grants.net().reduce(|all, net| all && net);

        let compiled = admitted.is_none().then(|| rules(grants)).flatten();
        let exec = admitted
            .map_or(compiled.as_ref(), Admitted::rules)
            .map_or(ExecProjection::Unrestricted, |rules| {
                ExecProjection::Restricted(rules.for_kernel(&carriers(rules, admitted)))
            });
        // Attenuated exec is worth an OS sandbox exactly where a backend
        // carries the rules into the kernel, which is the backends' fact to
        // state and `sandbox::EXEC_ENFORCED`'s to answer.  The in-process guard
        // runs on every platform regardless; the kernel layer is what sees the
        // re-execs it cannot (`sh -c`).
        let exec_triggers_sandbox =
            crate::sandbox::EXEC_ENFORCED && !matches!(exec, ExecProjection::Unrestricted);

        if regions.is_none() && net.is_none_or(|allowed| allowed) && !exec_triggers_sandbox {
            crate::dbg_trace!(
                "sandbox-proj",
                "fold unrestricted in {:?} (no OS sandbox needed)",
                t_fold.elapsed()
            );
            return None;
        }

        let fs = match regions {
            Some((read, write)) => FsProjection::Restricted(FsRules {
                read_prefixes: surface(read.live()),
                write_prefixes: surface(write.live()),
                // Both regions carry the same denies; a disk change between the two
                // folds can only drop a write allow, which fails closed.
                deny_paths: surface(read.denies()),
                pinned_dirs: Vec::new(),
            }),
            None => FsProjection::Unrestricted,
        };
        let projection = Self {
            fs,
            net: net.unwrap_or(true),
            exec,
        };
        crate::dbg_trace!("sandbox-proj", "fold restricted in {:?}", t_fold.elapsed());
        Some(projection)
    }
}

/// What the kernel must admit beside `rules` for its admitted files, and the
/// launched program, to start.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn carriers(rules: &ExecRules, admitted: Option<&Admitted>) -> BTreeSet<crate::path::RealPath> {
    let launched = admitted.and_then(|a| match a.program() {
        Program::File { real, .. } => Some(real),
        Program::Tool(_) => None,
    });
    super::carriers(rules.allowed_files().chain(launched))
}

/// No kernel exec layer here ([`crate::sandbox::EXEC_ENFORCED`]), so nothing
/// to carry.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn carriers(_: &ExecRules, _: Option<&Admitted>) -> BTreeSet<crate::path::RealPath> {
    BTreeSet::new()
}

/// The fs projection is lexical: `resolved` has no reader below this fold,
/// so each prefix flattens to its surface spelling here, once, and every
/// backend widens that into its own name class at render time.
fn surface<'a>(prefixes: impl Iterator<Item = &'a FrozenPath>) -> Vec<String> {
    let unique: BTreeSet<&str> = prefixes.map(FrozenPath::as_str).collect();
    unique.into_iter().map(str::to_owned).collect()
}

#[cfg(test)]
mod rendered_tests {
    use super::*;
    #[cfg(unix)]
    use crate::path::RealPath;

    /// The pins the traversal derives, through the real renderer — pins are
    /// now a function of *rendered* names, so this goes through
    /// [`SandboxProjection::rendered`] rather than calling the derivation
    /// bare.  Containment, not equality: the renderer is entitled to add
    /// spellings (on Windows a nonexistent `/repo` expands to the
    /// drive-qualified `\\?\C:\repo` alongside itself), so callers assert
    /// `as_str` containment, not the exact set.
    fn pinned(write: &[&str], deny: &[&str]) -> Vec<String> {
        let strings = |ps: &[&str]| ps.iter().copied().map(str::to_string).collect::<Vec<_>>();
        let projection = SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                read_prefixes: Vec::new(),
                write_prefixes: strings(write),
                deny_paths: strings(deny),
                pinned_dirs: Vec::new(),
            }),
            net: true,
            exec: ExecProjection::default(),
        };
        let out = projection.rendered().expect("ASCII paths render");
        out.fs
            .rules()
            .expect("restricted in, restricted out")
            .pinned_dirs
            .iter()
            .map(|r| r.as_str().to_string())
            .collect()
    }

    /// The write prefix root is itself a proper ancestor of a deep-enough
    /// deny, so it is pinned alongside the two intermediate directories —
    /// the case that closes `mv /repo /scratch/r`.
    #[test]
    fn pins_every_ancestor_within_the_write_prefix_including_its_root() {
        let dirs = pinned(&["/repo"], &["/repo/a/b/secret"]);
        for expect in ["/repo", "/repo/a", "/repo/a/b"] {
            assert!(dirs.iter().any(|d| d == expect), "got {dirs:?}");
        }
    }

    /// A read prefix is not a write prefix: only the latter can widen a deny's
    /// chain, so a read-only `/repo` pins nothing.
    #[test]
    fn pins_nothing_when_no_write_prefix_covers_the_denys_chain() {
        assert_eq!(pinned(&[], &["/repo/.ssh/id_rsa"]), Vec::<String>::new());
    }

    #[test]
    fn pins_nothing_for_a_deny_outside_every_prefix() {
        assert_eq!(pinned(&["/repo"], &["/etc/secret"]), Vec::<String>::new());
    }

    /// A supplied pin is not authority: the traversal recomputes the set from
    /// the denies and write prefixes, so a forged wire value neither survives
    /// nor suppresses the real one.
    #[test]
    fn a_carried_pin_is_replaced_by_the_derived_one() {
        let forged = SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                read_prefixes: Vec::new(),
                write_prefixes: vec!["/repo".to_string()],
                deny_paths: vec!["/repo/a/secret".to_string()],
                pinned_dirs: vec!["/somewhere/else".to_string()],
            }),
            net: true,
            exec: ExecProjection::default(),
        };
        let out = forged.rendered().expect("ASCII paths render");
        let dirs: Vec<&str> = out
            .fs
            .rules()
            .expect("restricted in, restricted out")
            .pinned_dirs
            .iter()
            .map(Rendered::as_str)
            .collect();
        // Containment, not equality: this goes through the real renderer, which
        // is entitled to add spellings — see [`pinned`].  What is asserted is
        // that the derived pins are present and the forged one reached neither
        // the output nor, under any spelling, the rule set.
        assert!(dirs.contains(&"/repo"), "got {dirs:?}");
        assert!(dirs.contains(&"/repo/a"), "got {dirs:?}");
        assert!(
            !dirs.iter().any(|d| d.contains("somewhere")),
            "a carried pin survived rendering: {dirs:?}"
        );
    }

    /// The reach of `write` over the admit `dir`, itself one of `admits`,
    /// through the real renderers: every spelling of the admit must agree, or
    /// the freeze would split one directory.
    #[cfg(unix)]
    fn reach(write: &[&str], admits: &[&str], dir: &str) -> WriteReach {
        let projection = SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                write_prefixes: write.iter().copied().map(str::to_string).collect(),
                ..FsRules::default()
            }),
            net: true,
            exec: ExecProjection::default(),
        };
        let rendered = projection.rendered().expect("ASCII paths render");
        let fs = rendered.fs.rules().expect("restricted in, restricted out");
        let names = |path: &str| {
            crate::path::render_real(&RealPath::assumed(path)).expect("ASCII path renders")
        };
        let admitted: Vec<Rendered> = admits.iter().flat_map(|admit| names(admit)).collect();
        let spellings = names(dir);
        let first = fs.write_reach(&spellings[0], &admitted);
        assert!(
            spellings
                .iter()
                .all(|spelling| fs.write_reach(spelling, &admitted) == first),
            "{dir} split across its spellings: {spellings:?}"
        );
        first
    }

    /// [`reach`] over `dir` as the only admit.
    #[cfg(unix)]
    fn reach_alone(write: &[&str], dir: &str) -> WriteReach {
        reach(write, &[dir], dir)
    }

    #[cfg(unix)]
    #[test]
    fn a_write_prefix_identical_to_the_admit_is_trusted() {
        assert_eq!(
            reach_alone(&["/ral-test/w"], "/ral-test/w"),
            WriteReach::Trusted
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_write_prefix_inside_the_admit_is_trusted() {
        assert_eq!(
            reach_alone(&["/ral-test/w/out"], "/ral-test/w"),
            WriteReach::Trusted
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_shallower_write_prefix_covers_the_admit() {
        assert_eq!(
            reach_alone(&["/ral-test"], "/ral-test/w/bin"),
            WriteReach::Covered
        );
        assert_eq!(reach_alone(&["/"], "/ral-test/w/bin"), WriteReach::Covered);
    }

    #[cfg(unix)]
    #[test]
    fn a_disjoint_write_prefix_leaves_the_admit_apart() {
        assert_eq!(
            reach_alone(&["/ral-test/other"], "/ral-test/w"),
            WriteReach::Apart
        );
        assert_eq!(reach_alone(&[], "/ral-test/w"), WriteReach::Apart);
    }

    /// Containment is by component: `/ral-test/w2` is no part of `/ral-test/w`.
    #[cfg(unix)]
    #[test]
    fn a_sibling_that_shares_a_name_prefix_is_apart() {
        assert_eq!(
            reach_alone(&["/ral-test/w2"], "/ral-test/w"),
            WriteReach::Apart
        );
    }

    /// Naming the tree outranks a shallower prefix that also reaches it.
    #[cfg(unix)]
    #[test]
    fn a_named_tree_stays_trusted_beside_a_covering_prefix() {
        assert_eq!(
            reach_alone(&["/ral-test", "/ral-test/w/bin"], "/ral-test/w/bin"),
            WriteReach::Trusted
        );
    }

    /// `write: cwd` with `cwd/` admitted: an admit nested under it, such as a
    /// `$PATH` entry inside the project, is written at or below an admit above
    /// it.
    #[cfg(unix)]
    #[test]
    fn an_admit_nested_in_a_trusted_admit_is_trusted() {
        let admits = ["/ral-test/proj", "/ral-test/proj/node_modules/.bin"];
        for dir in admits {
            assert_eq!(
                reach(&["/ral-test/proj"], &admits, dir),
                WriteReach::Trusted,
                "{dir}"
            );
        }
    }

    /// No admit holds both `~` and `~/.cargo/bin`, so a broad prefix over a
    /// lone admit stays covered.
    #[cfg(unix)]
    #[test]
    fn a_broad_prefix_over_a_lone_admit_stays_covered() {
        assert_eq!(
            reach_alone(&["/ral-test/home"], "/ral-test/home/.cargo/bin"),
            WriteReach::Covered
        );
    }

    /// Trust does not leak sideways: the carve-out `~/.cargo/registry` lies in
    /// `~/.cargo/`, which holds it, but does not reach `~/.cargo/bin`, and `~`
    /// lies within no admit.
    #[cfg(unix)]
    #[test]
    fn trust_does_not_leak_to_a_sibling_admit() {
        let write = ["/ral-test/home", "/ral-test/home/.cargo/registry"];
        let admits = ["/ral-test/home/.cargo", "/ral-test/home/.cargo/bin"];
        assert_eq!(reach(&write, &admits, admits[0]), WriteReach::Trusted);
        assert_eq!(reach(&write, &admits, admits[1]), WriteReach::Covered);
    }

    /// `/tmp` and `/private/tmp` are one directory, so a prefix spelled one
    /// way reaches an admit spelled the other.
    #[cfg(target_os = "macos")]
    #[test]
    fn firmlink_spellings_classify_alike() {
        assert_eq!(
            reach_alone(&["/tmp/ral-test"], "/private/tmp/ral-test/bin"),
            WriteReach::Covered
        );
        assert_eq!(
            reach_alone(&["/private/tmp/ral-test/bin"], "/tmp/ral-test/bin"),
            WriteReach::Trusted
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{ExecProjection, WriteReach};
    use crate::capability::{Capabilities, ExecGrant, ExecKey, ExecScope, FsPolicy, Verdict};
    use crate::path::{FrozenPath, render_real};

    /// A layer with an opinion on writes, on exec, or both.
    fn layer(write: Option<&str>, admit: Option<&str>) -> Capabilities {
        let prefix = FrozenPath::from_surface;
        Capabilities {
            fs: write.map(|w| FsPolicy {
                read_prefixes: vec![prefix(w)],
                write_prefixes: vec![prefix(w)],
                deny_paths: Vec::new(),
            }),
            exec: admit.map(|dir| ExecGrant([(ExecKey::Dir(prefix(dir)), Verdict::Allow)].into())),
            ..Capabilities::root()
        }
    }

    /// The reach of the write region the stack folds to over the one dir it
    /// admits.
    fn folded_reach(base: Capabilities, inner: Capabilities) -> WriteReach {
        let projection = crate::test_helper::core_shell().with_capabilities(base, |sh| {
            sh.with_capabilities(inner, |sh| sh.sandbox_projection().expect("restricted"))
        });
        let rendered = projection.rendered().expect("ASCII paths render");
        let fs = rendered.fs.rules().expect("a layer restricted fs");
        let ExecProjection::Restricted(rules) = &rendered.exec else {
            panic!("a layer restricted exec");
        };
        let dirs: Vec<_> = (rules.rules())
            .filter_map(|(scope, v)| match scope {
                ExecScope::Dir(path) if !v.is_denied() => Some(render_real(path)),
                _ => None,
            })
            .collect::<Result<Vec<_>, _>>()
            .expect("ASCII paths render")
            .concat();
        fs.write_reach(
            dirs.first().expect("the admitted dir reaches the rules"),
            &dirs,
        )
    }

    /// The base layer's prefix survives the fold when no inner layer says
    /// anything about writes, so the admit an inner layer adds is covered by a
    /// grant it never wrote.
    #[test]
    fn a_shallower_base_layer_covers_what_an_inner_layer_admits() {
        let reach = folded_reach(
            layer(Some("/ral-test/w"), None),
            layer(None, Some("/ral-test/w/bin")),
        );
        assert_eq!(reach, WriteReach::Covered);
    }

    /// The control: an inner layer naming the admit as a write prefix too
    /// meets to the deeper prefix, which is the admit itself.
    #[test]
    fn an_inner_layer_that_names_the_admit_as_writable_trusts_it() {
        let reach = folded_reach(
            layer(Some("/ral-test/w"), None),
            layer(Some("/ral-test/w/bin"), Some("/ral-test/w/bin")),
        );
        assert_eq!(reach, WriteReach::Trusted);
    }
}
