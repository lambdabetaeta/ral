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

use crate::path::{NormalizedPrefix, meet_prefixes};
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
        self || other
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
/// binaries resolving inside it — so the two-valued half *is* the partition,
/// and [`Meet`]/[`Join`] just intersect the allows and union the denies.
/// Literals beat dirs where both cover a candidate; among dirs, the deepest wins.
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

/// OS-renderable view of the meet-folded fs policy.  `Unrestricted` is the
/// lattice top — no layer attenuated fs, so the profile passes it through with
/// broad `file-read*`/`file-write*` on macOS, `--dev-bind / /` on Linux.  An
/// empty `Restricted` is the other extreme: fs was attenuated to nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", content = "policy", rename_all = "snake_case")]
pub enum FsProjection {
    #[default]
    Unrestricted,
    Restricted(FsPolicy),
}

impl FsProjection {
    /// The policy when restricted, `None` at the unrestricted top.  Renderers
    /// wanting only the prefixes match on this; the macOS profile builder
    /// branches on the variant, since the two emit different SBPL shapes.
    pub fn as_policy(&self) -> Option<&FsPolicy> {
        match self {
            Self::Unrestricted => None,
            Self::Restricted(p) => Some(p),
        }
    }
}

/// OS-renderable view of the meet-folded exec policy.  Under `Unrestricted`
/// the in-ral gate is the only check; `Restricted` closes the OS layer around
/// the same admits, shutting the `sh -c "PATH=…; cmd"` route by which a
/// sandboxed child re-execs binaries the gate never sees.  An empty
/// `Restricted` admits nothing and the deny-default kills every spawn.
///
/// The three deny dimensions mirror the three shapes of the in-ral veto, so the
/// profile denies exactly what the gate would.  `deny_basenames` renders as a
/// final-path-component match: a bare-name deny must hold wherever the name
/// resolves, and must not be dodged by reaching it through an admitted dir.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecProjection {
    #[default]
    Unrestricted,
    Restricted {
        allow_paths: Vec<String>,
        allow_dirs: Vec<String>,
        #[serde(default)]
        deny_paths: Vec<String>,
        #[serde(default)]
        deny_dirs: Vec<String>,
        #[serde(default)]
        deny_basenames: Vec<String>,
    },
}

/// The OS-renderable projection of the effective grant, produced by
/// `sandbox_projection` in `core/src/capability/sandbox.rs` after meet-folding
/// the whole stack.  The platform backends `sandbox::linux` and
/// `sandbox::macos` render it, and it rides the internal
/// `--sandbox-projection` flag to a re-exec'd child.  Unlike a
/// [`Capabilities`] frame, no further composition can widen it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxProjection {
    #[serde(default)]
    pub fs: FsProjection,
    pub net: bool,
    #[serde(default)]
    pub exec: ExecProjection,
}

impl Default for SandboxProjection {
    fn default() -> Self {
        Self {
            fs: FsProjection::default(),
            net: true,
            exec: ExecProjection::default(),
        }
    }
}

/// Lexical view of the projection — prefixes as written, for the Seatbelt and
/// bwrap profile renderers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxBindSpec {
    pub read_prefixes: Vec<String>,
    pub write_prefixes: Vec<String>,
    pub deny_paths: Vec<String>,
}

impl SandboxProjection {
    /// Empty when fs is `Unrestricted`: there the renderer emits broad allows,
    /// not per-prefix rules.
    pub fn bind_spec(&self) -> SandboxBindSpec {
        let Some(fs) = self.fs.as_policy() else {
            return SandboxBindSpec::default();
        };
        let dedup = |ps: &[NormalizedPrefix]| {
            ps.iter()
                .map(|p| p.as_str().to_string())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        };
        SandboxBindSpec {
            read_prefixes: dedup(&fs.read_prefixes),
            write_prefixes: dedup(&fs.write_prefixes),
            deny_paths: dedup(&fs.deny_paths),
        }
    }
}

/// Gates the `_ed-*` builtins.  `deny_unknown_fields` is structural: TOML
/// attaches every key after a header to that header, so a stray top-level key
/// drifting into `[editor]` must error rather than be silently dropped.
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
/// `grant { ... }` on top.  A newtype so the folds over it live together rather
/// than being respelled as `iter().any(...)` at each call site; `transparent`
/// serde keeps it free at every boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GrantStack(Vec<Capabilities>);

impl GrantStack {
    /// Where every shell starts; `grant { ... }` blocks and the session-wide
    /// `--capabilities` ceiling push attenuating layers on top.
    pub fn root() -> Self {
        Self(vec![Capabilities::root()])
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

fn union_prefixes(a: Vec<NormalizedPrefix>, b: Vec<NormalizedPrefix>) -> Vec<NormalizedPrefix> {
    a.into_iter()
        .chain(b)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod lattice_tests;
