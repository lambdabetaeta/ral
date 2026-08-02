//! The capability lattice: one frame of typed authority, and the folds over it.
//!
//! A [`Capabilities`] frame bundles the per-effect policies plus an `audit`
//! flag.  [`Capabilities::meet`] composes frames downward — [`Capabilities::root`]
//! is top, [`Capabilities::deny_all`] bottom — and [`Capabilities::join`] widens a
//! base ceiling at load time.  Denies are sticky under both.
//!
//! [`SandboxProjection`] is the meet-folded fs+net+exec residue the OS sandbox
//! backends render; `detach` gates a verb instead of an OS rule, so it is folded
//! at the call by [`GrantStack::permits_detach`] and reaches no projection.

use crate::path::{
    NormalizedPrefix, Rendered, meet_prefixes, path_within_str, proper_ancestors, render_paths,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

// ── Lattice traits ────────────────────────────────────────────────────────
//
// The two semilattice operations, beside the types they run over.  The
// `Option<T>` and `bool` lifts ride along because `None` as identity — a layer
// with no opinion on a field — is a capability convention, not a universal one.

/// Greatest lower bound: the most-authority value below both sides.
/// Commutative, associative and idempotent, per type in `lattice_tests`.
pub trait Meet {
    fn meet(self, other: Self) -> Self;
}

/// Widen a base ceiling with an extension, before any attenuation runs.
///
/// Not the order-dual of [`Meet`]: vetoes survive from either side, so a deny
/// is lifted by choosing a different base, never by composing over it.
pub trait Join {
    fn join(self, other: Self) -> Self;
}

impl<T: Meet> Meet for Option<T> {
    fn meet(self, other: Self) -> Self {
        match (self, other) {
            (None, x) | (x, None) => x,
            (Some(a), Some(b)) => Some(a.meet(b)),
        }
    }
}

impl<T: Join> Join for Option<T> {
    fn join(self, other: Self) -> Self {
        match (self, other) {
            (None, x) | (x, None) => x,
            (Some(a), Some(b)) => Some(a.join(b)),
        }
    }
}

impl Meet for bool {
    fn meet(self, other: Self) -> Self {
        self && other
    }
}

impl Join for bool {
    fn join(self, other: Self) -> Self {
        self && other
    }
}

/// Exec verdict for one key of [`ExecMap::literals`] — a bare command name
/// (`git`) or an absolute path (`/usr/bin/git`).
///
/// A three-point lattice: `Allow` on top, `Subcommands` between (more elements,
/// more authority), `Deny` at bottom.  A `Deny` is sticky under meet *and*
/// join, even against a layer whose map omits the key, so a base can pin a name
/// out once and no overlay re-grants it; only a different base lifts one.
/// Literal keys beat the directory prefixes in the same [`ExecMap`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecPolicy {
    Allow,
    /// Admits only these first arguments.  `BTreeSet` canonicity is what makes
    /// `meet` and `join` idempotent with no normalization pass.
    Subcommands(BTreeSet<String>),
    Deny,
}

/// Exec authority partitioned by key kind: `literals` carry the full
/// three-valued [`ExecPolicy`], while a directory can only admit or deny the
/// binaries resolving inside it.
///
/// The two-valued half *is* the partition, so [`Meet`]/[`Join`] just
/// intersect the allows and union the denies. Literals beat dirs where both
/// cover a candidate; among dirs, the deepest wins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecMap {
    #[serde(default)]
    pub literals: BTreeMap<String, ExecPolicy>,
    #[serde(default)]
    pub allow_dirs: BTreeSet<NormalizedPrefix>,
    #[serde(default)]
    pub deny_dirs: BTreeSet<NormalizedPrefix>,
}

/// Filesystem access within a `grant` block.
///
/// `deny_paths` carve out subtrees no read, write, link, or rename may touch
/// even under a covering prefix, matched as subpaths — a file denies itself, a
/// directory everything beneath it.  This is what keeps the agent's own
/// capability profile unwritable inside an otherwise-writable cwd, and the
/// credential dirs (`xdg:config/gh`, `xdg:config/op`, …) unreadable under a
/// wholesale-readable `xdg:config`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsPolicy {
    #[serde(default)]
    pub read_prefixes: Vec<NormalizedPrefix>,
    #[serde(default)]
    pub write_prefixes: Vec<NormalizedPrefix>,
    #[serde(default)]
    pub deny_paths: Vec<NormalizedPrefix>,
}

/// The fs half of a projection, its paths named in `N`.
///
/// Not [`FsPolicy`]: that is the grant-layer lattice element, whose
/// [`NormalizedPrefix`]es carry the `resolved` and `namespace` forms the meet
/// keys on.  Nothing below the fold reads those, so the projection holds plain
/// surface spellings and each backend widens them into its own name class at
/// render time — which is exactly what `N` is.  Flattening away `namespace`
/// forecloses a projection that distinguishes guest prefixes from host ones;
/// no backend ever saw that distinction, so enforcement is unchanged.
///
/// `pinned_dirs` is `serde(skip)` because it is *derived*, never authored:
/// [`SandboxProjection::traverse`] mints it from `deny_paths` and
/// `write_prefixes`, so a forged `--sandbox-projection` can neither fabricate
/// a pin nor drop one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
// Spelled out because the skipped field would otherwise drag an `N: Default`
// bound onto the impl, which a name minted only by expansion cannot meet.
#[serde(deny_unknown_fields, bound(deserialize = "N: Deserialize<'de>"))]
pub struct FsRules<N> {
    #[serde(default)]
    pub read_prefixes: Vec<N>,
    #[serde(default)]
    pub write_prefixes: Vec<N>,
    #[serde(default)]
    pub deny_paths: Vec<N>,
    /// Every proper ancestor of a `deny_paths` entry that lies within some
    /// write prefix.  The macOS backend pins each against rename and unlink,
    /// or a confined child relocates the ancestor directory itself
    /// (`mv /repo/.ssh /repo/x`, or `mv /repo /scratch/r` when the write
    /// prefix root is the ancestor) and the denied bytes resurface at a name
    /// no deny rule covers.  `deny_paths` already carries both a deny's
    /// surface spelling and its symlink-resolved target
    /// (`sandbox_projection` in `capability::sandbox`), so taking ancestors
    /// of it pins both chains — closing a symlink swapped in after sandbox
    /// entry too.
    #[serde(skip)]
    pub pinned_dirs: Vec<N>,
}

/// Empty under any naming: rules over no paths, the shape a backend falls back
/// to at the unrestricted top.  Hand-written for the same reason as the serde
/// bound above — the derive would demand `N: Default`, which a name minted only
/// by expansion cannot meet.
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

/// OS-renderable view of the meet-folded fs policy.
///
/// `Unrestricted` is the lattice top — no layer attenuated fs, so the profile
/// passes it through with broad `file-read*`/`file-write*` on macOS,
/// `--dev-bind / /` on Linux. An empty `Restricted` is the other extreme: fs
/// was attenuated to nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "rules", rename_all = "snake_case")]
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
    pub fn rules(&self) -> Option<&FsRules<N>> {
        match self {
            Self::Unrestricted => None,
            Self::Restricted(r) => Some(r),
        }
    }
}

/// OS-renderable view of the meet-folded exec policy.
///
/// Under `Unrestricted` the in-ral gate is the only check; `Restricted`
/// closes the OS layer around the same admits, shutting the `sh -c
/// "PATH=…; cmd"` route by which a sandboxed child re-execs binaries the
/// gate never sees. An empty `Restricted` admits nothing and the
/// deny-default kills every spawn.
///
/// The three deny dimensions mirror the three shapes of the in-ral veto, so the
/// profile denies exactly what the gate would.  `deny_basenames` renders as a
/// final-path-component match: a bare-name deny must hold wherever the name
/// resolves, and must not be dodged by reaching it through an admitted dir.
///
/// `deny_basenames` stays `Vec<String>` while every path set is `Vec<N>`, and
/// that asymmetry is load-bearing: a bare name is not a path, so under a
/// rendered naming expanding it — or sliding it into a path set — stops
/// typechecking rather than quietly emitting a rule for `/git`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecProjection<N = String> {
    Unrestricted,
    Restricted {
        allow_paths: Vec<N>,
        allow_dirs: Vec<N>,
        #[serde(default)]
        deny_paths: Vec<N>,
        #[serde(default)]
        deny_dirs: Vec<N>,
        #[serde(default)]
        deny_basenames: Vec<String>,
    },
}

/// Hand-written for the same reason as [`FsProjection`]'s.
impl<N> Default for ExecProjection<N> {
    fn default() -> Self {
        Self::Unrestricted
    }
}

/// The OS-renderable projection of the effective grant, produced by
/// `sandbox_projection` in `core/src/capability/sandbox.rs` after meet-folding
/// the whole stack.
///
/// The platform backends `sandbox::linux` and `sandbox::macos` render it,
/// and it rides the internal `--sandbox-projection` flag to a re-exec'd
/// child. Unlike a [`Capabilities`] frame, no further composition can widen
/// it.
///
/// `N` is how the projection *names* the objects it rules over.  Only the
/// surface instance crosses the wire, and structurally so: the derives
/// generate `impl<N: Serialize>` bounds while [`Rendered`] implements neither
/// serde trait, so shipping one host's expansion of one host's filesystem into
/// another's rules does not compile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxProjection<N = String> {
    #[serde(default)]
    pub fs: FsProjection<N>,
    pub net: bool,
    #[serde(default)]
    pub exec: ExecProjection<N>,
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
    /// Rename every path in the projection through `f`, ordering and deduping
    /// each set once on the way and deriving `pinned_dirs` from the result.
    ///
    /// The completeness guarantee is structural, not promised: the input is
    /// destructured exhaustively, the output constructed exhaustively, and the
    /// only way to obtain a `Vec<Rendered>` is to call `f`.  A path set added
    /// later therefore fails to compile until it too is threaded — which is
    /// the point, since every under-enforcement of this class has been someone
    /// forgetting to expand one new list.
    ///
    /// Pins are derived here rather than carried because they are a *function*
    /// of the deny paths and the write prefixes, taken in surface space where
    /// containment is the same lexical judgment the fold already made, and
    /// only then handed to `f` alongside the sets they came from.
    ///
    /// # Errors
    ///
    /// Whatever `f` refuses; [`render_paths`] refuses a name whose expansion
    /// it cannot spell faithfully.
    pub fn traverse(
        &self,
        f: impl Fn(&[String]) -> Result<Vec<Rendered>, String>,
    ) -> Result<SandboxProjection<Rendered>, String> {
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
                let write_prefixes = ordered(write_prefixes);
                let deny_paths = ordered(deny_paths);
                let pinned_dirs = derive_pins(&deny_paths, &write_prefixes);
                FsProjection::Restricted(FsRules {
                    read_prefixes: f(&ordered(read_prefixes))?,
                    write_prefixes: f(&write_prefixes)?,
                    deny_paths: f(&deny_paths)?,
                    pinned_dirs: f(&pinned_dirs)?,
                })
            }
        };
        let exec = match exec {
            ExecProjection::Unrestricted => ExecProjection::Unrestricted,
            ExecProjection::Restricted {
                allow_paths,
                allow_dirs,
                deny_paths,
                deny_dirs,
                deny_basenames,
            } => ExecProjection::Restricted {
                allow_paths: f(&ordered(allow_paths))?,
                allow_dirs: f(&ordered(allow_dirs))?,
                deny_paths: f(&ordered(deny_paths))?,
                deny_dirs: f(&ordered(deny_dirs))?,
                // A bare name reaches no expansion, and the differing types
                // make that a compile-time fact rather than a convention.
                deny_basenames: ordered(deny_basenames),
            },
        };
        Ok(SandboxProjection {
            fs,
            net: *net,
            exec,
        })
    }

    /// [`traverse`](Self::traverse) under this host's own name-class
    /// expansion — the one call a backend makes, after which no rule it emits
    /// can name a spelling the kernel will not present.
    ///
    /// # Errors
    ///
    /// As [`render_paths`].
    pub fn rendered(&self) -> Result<SandboxProjection<Rendered>, String> {
        self.traverse(render_paths)
    }
}

/// Gates the `_ed-*` builtins.
///
/// `deny_unknown_fields` is structural: TOML attaches every key after a
/// header to that header, so a stray top-level key drifting into `[editor]`
/// must error rather than be silently dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorPolicy {
    pub read: bool,
    pub write: bool,
    pub tui: bool,
}

/// Gates the `cd` builtin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellPolicy {
    pub chdir: bool,
}

/// One layer of the dynamic grant stack — per-effect policies plus an `audit`
/// flag, with every `~` / `xdg:` / `cwd:` / `tempdir:` sigil already resolved to
/// a concrete path.
///
/// Resolved *by construction*: the only non-trivial constructor is
/// `decode_capability_map` in `core/src/capability/decode.rs`, which resolves
/// every sigil against a `FreezeCtx { home, cwd }` before returning, and the
/// remaining ways in ([`root`](Self::root), [`deny_all`](Self::deny_all),
/// `default`) are path-free.  So the `Serialize` impls can back `WireContext`
/// in `subprocess.rs` unguarded: a re-exec'd child inherits a stack whose paths
/// the parent already pinned.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub exec: Option<ExecMap>,
    #[serde(default)]
    pub fs: Option<FsPolicy>,
    #[serde(default)]
    pub net: Option<bool>,
    /// Authority to birth a process this session stops owning.  `None`
    /// inherits, so silence permits as on every other axis: a `grant` that
    /// attenuates fs says nothing about survivors.
    #[serde(default)]
    pub detach: Option<bool>,
    #[serde(default)]
    pub audit: bool,
    #[serde(default)]
    pub editor: Option<EditorPolicy>,
    #[serde(default)]
    pub shell: Option<ShellPolicy>,
}

/// The dynamic stack of capability layers: ambient root at index 0, innermost
/// `grant { ... }` on top.
///
/// A newtype so the folds over it live together rather than being respelled
/// as `iter().any(...)` at each call site; `transparent` serde keeps it free
/// at every boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GrantStack(Vec<Capabilities>);

impl GrantStack {
    /// Where every shell starts; `grant { ... }` blocks and the session-wide
    /// `--capabilities` ceiling push attenuating layers on top.
    pub fn root() -> Self {
        Self(vec![Capabilities::root()])
    }

    /// A stack of exactly `frame`: a view for asking a gate's question of one
    /// frame — a boot-time `Capabilities` no shell holds yet — not a session
    /// stack, which is always built by [`GrantStack::root`] plus `push`.
    /// Verdict-identical to `[root, frame]` because the ambient root holds no
    /// opinion on any axis.
    pub fn of(frame: Capabilities) -> Self {
        Self(vec![frame])
    }

    /// True iff a real grant sits above the ambient root — what
    /// `Shell::has_active_capabilities` reports.
    pub fn is_restrictive(&self) -> bool {
        self.0.iter().any(Capabilities::is_restrictive)
    }

    /// True iff some layer opts into capability-check audit emission.  Only
    /// half the gate — pair it with `Audit::active`.
    pub fn any_audits(&self) -> bool {
        self.0.iter().any(|ctx| ctx.audit)
    }

    pub fn push(&mut self, layer: Capabilities) {
        self.0.push(layer);
    }

    pub fn pop(&mut self) -> Option<Capabilities> {
        self.0.pop()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Capabilities> {
        self.0.iter()
    }

    /// Layers with no opinion are skipped, here and in `fs` and `net` below:
    /// the folds intersect what remains, and an empty run means no attenuation.
    pub fn exec(&self) -> impl Iterator<Item = &ExecMap> {
        self.0.iter().filter_map(|c| c.exec.as_ref())
    }

    pub fn fs(&self) -> impl Iterator<Item = &FsPolicy> {
        self.0.iter().filter_map(|c| c.fs.as_ref())
    }

    pub fn net(&self) -> impl Iterator<Item = bool> {
        self.0.iter().filter_map(|c| c.net)
    }

    /// A meet over the layers, so one `detach: false` anywhere withholds the
    /// verb whatever sits above it.  Folded here and not into
    /// [`SandboxProjection`] because it decides only whether the survivor is
    /// born; what it inherits is the projection of the frame it was born in.
    pub fn permits_detach(&self) -> bool {
        self.0.iter().all(|c| c.detach != Some(false))
    }
}

impl<'a> IntoIterator for &'a GrantStack {
    type Item = &'a Capabilities;
    type IntoIter = std::slice::Iter<'a, Capabilities>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl Capabilities {
    /// Lattice bottom for positive authority: every effect pinned to its
    /// most-restrictive value, so a `meet` against it zeroes every dimension.
    pub fn deny_all() -> Self {
        Self {
            exec: Some(ExecMap::default()),
            fs: Some(FsPolicy::default()),
            net: Some(false),
            detach: Some(false),
            editor: Some(EditorPolicy::default()),
            shell: Some(ShellPolicy::default()),
            audit: false,
        }
    }

    /// True iff some effect is `Some(_)` rather than the inheriting `None`.
    pub fn is_restrictive(&self) -> bool {
        self.exec.is_some()
            || self.fs.is_some()
            || self.net.is_some()
            || self.detach.is_some()
            || self.editor.is_some()
            || self.shell.is_some()
    }

    /// Ambient authority, the lattice top: all fields `None`, no attenuation.
    pub fn root() -> Self {
        Self::default()
    }
}

impl Capabilities {
    /// The most-authority capability below both sides.  Inner fields intersect
    /// (exec maps, fs prefixes) or AND (net, detach, editor, shell), while
    /// `fs.deny_paths` unions — more denies is less authority.  `audit` sits
    /// outside the lattice and propagates upward.  Prefix intersection goes
    /// through [`meet_prefixes`], judged on the `resolved` form each
    /// [`NormalizedPrefix`] already carries, so no disk is consulted here.
    pub fn meet(self, other: Self) -> Self {
        Self {
            exec: self.exec.meet(other.exec),
            fs: self.fs.meet(other.fs),
            net: self.net.meet(other.net),
            detach: self.detach.meet(other.detach),
            editor: self.editor.meet(other.editor),
            shell: self.shell.meet(other.shell),
            audit: self.audit || other.audit,
        }
    }

    /// Widen `self` with `other` — the composition `--extend-base` runs to lift
    /// a base ceiling before any attenuation.  Positive authority unions, but
    /// every veto survives from either side, so an extension can grant where
    /// the base was silent and never re-admit what it denied.  Hence no
    /// order-dual of [`meet`](Self::meet): a deny is a floor under both.
    pub fn join(self, other: Self) -> Self {
        Self {
            exec: self.exec.join(other.exec),
            fs: self.fs.join(other.fs),
            net: self.net.join(other.net),
            detach: self.detach.join(other.detach),
            editor: self.editor.join(other.editor),
            shell: self.shell.join(other.shell),
            audit: self.audit || other.audit,
        }
    }
}

// ── Lattice impls ─────────────────────────────────────────────────────────
//
// The `literals` folds stay free fns below: they range over a whole map, which
// the per-element trait cannot see.

impl ExecPolicy {
    /// `name`, or `name[sub1,sub2,…]` under `Subcommands`; `None` when denied.
    /// The set iterates sorted, so the label is deterministic.
    pub fn admit_label(&self, name: &str) -> Option<String> {
        match self {
            Self::Allow => Some(name.to_string()),
            Self::Subcommands(subs) => Some(format!(
                "{name}[{}]",
                subs.iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            )),
            Self::Deny => None,
        }
    }

    pub fn is_denied(&self) -> bool {
        matches!(self, Self::Deny)
    }
}

impl Meet for ExecPolicy {
    fn meet(self, other: Self) -> Self {
        match (self, other) {
            (Self::Deny, _) | (_, Self::Deny) => Self::Deny,
            (Self::Allow, Self::Allow) => Self::Allow,
            (Self::Allow, Self::Subcommands(s)) | (Self::Subcommands(s), Self::Allow) => {
                Self::Subcommands(s)
            }
            (Self::Subcommands(s1), Self::Subcommands(s2)) => Self::Subcommands(&s1 & &s2),
        }
    }
}

impl Join for ExecPolicy {
    fn join(self, other: Self) -> Self {
        match (self, other) {
            // Deny-overrides under widening too, so `--extend-base` can never
            // re-admit a command the base denies.
            (Self::Deny, _) | (_, Self::Deny) => Self::Deny,
            (Self::Allow, _) | (_, Self::Allow) => Self::Allow,
            (Self::Subcommands(s1), Self::Subcommands(s2)) => Self::Subcommands(&s1 | &s2),
        }
    }
}

impl Meet for FsPolicy {
    fn meet(self, other: Self) -> Self {
        let sorted_meet = |a: &[NormalizedPrefix], b: &[NormalizedPrefix]| {
            let mut out = meet_prefixes(a, b);
            out.sort();
            out.dedup();
            out
        };
        Self {
            read_prefixes: sorted_meet(&self.read_prefixes, &other.read_prefixes),
            write_prefixes: sorted_meet(&self.write_prefixes, &other.write_prefixes),
            deny_paths: union_prefixes(self.deny_paths, other.deny_paths),
        }
    }
}

impl Join for FsPolicy {
    fn join(self, other: Self) -> Self {
        Self {
            read_prefixes: union_prefixes(self.read_prefixes, other.read_prefixes),
            write_prefixes: union_prefixes(self.write_prefixes, other.write_prefixes),
            // Denies union exactly as in `meet`: an overlay silent on a base
            // carve-out must not lift it.
            deny_paths: union_prefixes(self.deny_paths, other.deny_paths),
        }
    }
}

impl Meet for EditorPolicy {
    fn meet(self, other: Self) -> Self {
        Self {
            read: self.read.meet(other.read),
            write: self.write.meet(other.write),
            tui: self.tui.meet(other.tui),
        }
    }
}

impl Join for EditorPolicy {
    fn join(self, other: Self) -> Self {
        Self {
            read: self.read.join(other.read),
            write: self.write.join(other.write),
            tui: self.tui.join(other.tui),
        }
    }
}

impl Meet for ShellPolicy {
    fn meet(self, other: Self) -> Self {
        Self {
            chdir: self.chdir.meet(other.chdir),
        }
    }
}

impl Join for ShellPolicy {
    fn join(self, other: Self) -> Self {
        Self {
            chdir: self.chdir.join(other.chdir),
        }
    }
}

/// `allow_dirs` intersects through [`meet_prefixes`] — a prefix survives only
/// where both sides admit it, the deeper winning — while `deny_dirs` unions.
/// The sweep afterwards drops any allow [`same_gate_dir`](NormalizedPrefix::same_gate_dir)
/// as a deny rather than merely byte-equal to it, so a clash resolves to the
/// deny even across alias spellings or two sides that froze different disk state.
impl Meet for ExecMap {
    fn meet(self, other: Self) -> Self {
        let self_allow: Vec<NormalizedPrefix> = self.allow_dirs.into_iter().collect();
        let other_allow: Vec<NormalizedPrefix> = other.allow_dirs.into_iter().collect();
        let mut allow_dirs: BTreeSet<NormalizedPrefix> = meet_prefixes(&self_allow, &other_allow)
            .into_iter()
            .collect();
        let deny_dirs: BTreeSet<NormalizedPrefix> =
            self.deny_dirs.into_iter().chain(other.deny_dirs).collect();
        allow_dirs.retain(|p| !deny_dirs.iter().any(|d| d.same_gate_dir(p)));
        Self {
            allow_dirs,
            deny_dirs,
            literals: meet_literal_exec(&self.literals, &other.literals),
        }
    }
}

/// Both sets union, then the same `same_gate_dir` sweep as [`meet`](Meet::meet)
/// drops the allows a deny covers — so an overlay that re-grants a directory
/// the base vetoed still loses it.
impl Join for ExecMap {
    fn join(self, other: Self) -> Self {
        let mut allow_dirs: BTreeSet<NormalizedPrefix> = self
            .allow_dirs
            .into_iter()
            .chain(other.allow_dirs)
            .collect();
        let deny_dirs: BTreeSet<NormalizedPrefix> =
            self.deny_dirs.into_iter().chain(other.deny_dirs).collect();
        allow_dirs.retain(|p| !deny_dirs.iter().any(|d| d.same_gate_dir(p)));
        Self {
            allow_dirs,
            deny_dirs,
            literals: join_literal_exec(&self.literals, &other.literals),
        }
    }
}

/// Allow-sided keys must appear on both sides; a `Deny` propagates from either,
/// even where the other map has no entry at all.
fn meet_literal_exec(
    a: &BTreeMap<String, ExecPolicy>,
    b: &BTreeMap<String, ExecPolicy>,
) -> BTreeMap<String, ExecPolicy> {
    let mut out = BTreeMap::new();
    for (name, pa) in a {
        match b.get(name) {
            Some(pb) => {
                out.insert(name.clone(), pa.clone().meet(pb.clone()));
            }
            None if matches!(pa, ExecPolicy::Deny) => {
                out.insert(name.clone(), ExecPolicy::Deny);
            }
            None => {}
        }
    }
    for (name, pb) in b {
        if a.contains_key(name) {
            continue;
        }
        if matches!(pb, ExecPolicy::Deny) {
            out.insert(name.clone(), ExecPolicy::Deny);
        }
    }
    out
}

/// Shared keys combine through [`ExecPolicy::join`], and a one-sided key
/// survives verbatim: an absent key is the join identity, so silence on one
/// side lifts neither the other's grant nor its veto.
fn join_literal_exec(
    a: &BTreeMap<String, ExecPolicy>,
    b: &BTreeMap<String, ExecPolicy>,
) -> BTreeMap<String, ExecPolicy> {
    let mut out = BTreeMap::new();
    for (name, pa) in a {
        match b.get(name) {
            Some(pb) => {
                out.insert(name.clone(), pa.clone().join(pb.clone()));
            }
            None => {
                out.insert(name.clone(), pa.clone());
            }
        }
    }
    for (name, pb) in b {
        if !a.contains_key(name) {
            out.insert(name.clone(), pb.clone());
        }
    }
    out
}

/// The directories a deny needs kept traversable to be reachable at all:
/// every proper ancestor of a deny path that lies within some write prefix.
///
/// Surface space, before any host expansion.  Containment here is the same
/// lexical judgment the fold already made, and an ancestor chain is a property
/// of the name rather than of the object — so expanding first would only ask
/// the question once per spelling and get the same answer each time.
fn derive_pins(deny_paths: &[String], write_prefixes: &[String]) -> Vec<String> {
    proper_ancestors(deny_paths.iter().map(String::as_str))
        .into_iter()
        .filter(|dir| write_prefixes.iter().any(|w| path_within_str(dir, w)))
        .collect()
}

fn union_prefixes(a: Vec<NormalizedPrefix>, b: Vec<NormalizedPrefix>) -> Vec<NormalizedPrefix> {
    a.into_iter()
        .chain(b)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod lattice_tests;

#[cfg(test)]
mod traverse_tests {
    use super::*;

    /// The pins the traversal derives, in the surface space it derives them in.
    ///
    /// Deliberately short of the renderer: which ancestors the derivation picks
    /// is this module's concern, while how many names each one answers to is
    /// the host's.  Asserting on rendered output would conflate the two and
    /// make the expected value a property of the machine — on Windows a
    /// nonexistent `/repo` expands to the drive-qualified `\\?\C:\repo`
    /// alongside itself, where on Unix it expands to only itself.
    fn pinned(write: &[&str], deny: &[&str]) -> Vec<String> {
        let strings = |ps: &[&str]| ps.iter().copied().map(str::to_string).collect::<Vec<_>>();
        derive_pins(&strings(deny), &strings(write))
    }

    /// The write prefix root is itself a proper ancestor of a deep-enough
    /// deny, so it is pinned alongside the two intermediate directories —
    /// the case that closes `mv /repo /scratch/r`.
    #[test]
    fn pins_every_ancestor_within_the_write_prefix_including_its_root() {
        assert_eq!(
            pinned(&["/repo"], &["/repo/a/b/secret"]),
            ["/repo", "/repo/a", "/repo/a/b"]
        );
    }

    /// A read prefix is not a write prefix: only the latter can widen a deny's
    /// chain, so a read-only `/repo` pins nothing.
    #[test]
    fn pins_nothing_when_no_write_prefix_covers_the_denys_chain() {
        assert!(pinned(&[], &["/repo/.ssh/id_rsa"]).is_empty());
    }

    #[test]
    fn pins_nothing_for_a_deny_outside_every_prefix() {
        assert!(pinned(&["/repo"], &["/etc/secret"]).is_empty());
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

    /// Bare names are not paths: they pass the traversal untouched, never
    /// gaining the extra spellings a path would.
    #[test]
    fn a_denied_basename_is_carried_across_unexpanded() {
        let out = SandboxProjection {
            exec: ExecProjection::Restricted {
                allow_paths: Vec::new(),
                allow_dirs: Vec::new(),
                deny_paths: Vec::new(),
                deny_dirs: Vec::new(),
                deny_basenames: vec!["git".to_string()],
            },
            ..SandboxProjection::default()
        }
        .rendered()
        .expect("no paths to render");
        assert!(matches!(
            out.exec,
            ExecProjection::Restricted { deny_basenames, .. } if deny_basenames == ["git"]
        ));
    }
}
