//! The capability lattice: one frame of typed authority, and the folds over it.
//!
//! A [`Capabilities`] frame bundles the per-effect policies.  Composition
//! downward is the [`GrantStack`]: a `grant { ... }` or a loaded profile
//! pushes one more layer, and every verdict folds the layers afresh —
//! `capability::exec::rules`, `capability::fs::region`, `permits_detach` —
//! rather than flattening them into one `Capabilities` first.
//! [`Capabilities::widen`] is the one place a frame still composes eagerly:
//! `--extend-base` widens a base ceiling at load time, before any attenuation
//! runs, and its "silence lifts no veto" semantics only make sense as a
//! single-layer union.  Denies are sticky under both regimes.
//!
//! [`SandboxProjection`] is the meet-folded fs+net+exec residue the OS sandbox
//! backends render; `detach` gates a verb instead of an OS rule, so it is folded
//! at the call by [`GrantStack::permits_detach`] and reaches no projection.

use crate::path::{NormalizedPrefix, RealPath, Rendered, render_paths, rendered_pins};
use serde::{Deserialize, Serialize};
#[cfg(target_os = "macos")]
use std::borrow::Borrow;
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
pub trait Widen {
    fn widen(self, other: Self) -> Self;
}

impl<T: Meet> Meet for Option<T> {
    fn meet(self, other: Self) -> Self {
        match (self, other) {
            (None, x) | (x, None) => x,
            (Some(a), Some(b)) => Some(a.meet(b)),
        }
    }
}

impl<T: Widen> Widen for Option<T> {
    fn widen(self, other: Self) -> Self {
        match (self, other) {
            (None, x) | (x, None) => x,
            (Some(a), Some(b)) => Some(a.widen(b)),
        }
    }
}

impl Meet for bool {
    fn meet(self, other: Self) -> Self {
        self && other
    }
}

impl Widen for bool {
    fn widen(self, other: Self) -> Self {
        self && other
    }
}

/// `Deny < Only(s) < Allow`.
///
/// A `Deny` is sticky under meet *and* widen, even against a layer whose grant
/// omits the key, so a base can pin a command out once and no overlay
/// re-grants it; only a different base lifts one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    Allow,
    /// Admits only these first arguments.  `BTreeSet` canonicity is what makes
    /// `meet` and `widen` idempotent with no normalization pass.
    Only(BTreeSet<String>),
    Deny,
}

/// An exec key as the author wrote it: the written twin of
/// `capability::exec::ExecScope`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ExecKey {
    /// A bare name: `git`.
    Name(String),
    /// A path, frozen: `/usr/bin/git`, `~/bin/x`.
    Path(NormalizedPrefix),
    /// A directory, frozen, and the expansions of `path:` and `system:`.
    Dir(NormalizedPrefix),
}

/// The key as a grant spells it: a dir with its trailing `/`.
impl std::fmt::Display for ExecKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Name(name) => f.write_str(name),
            Self::Path(path) => f.write_str(path.as_str()),
            Self::Dir(dir) => write!(f, "{}/", dir.as_str()),
        }
    }
}

/// Exec authority as authored, per layer.
///
/// The author's spellings survive for display; `capability::exec` compiles
/// the grant into rules over programs, and nothing matches it directly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecGrant(#[serde(with = "pairs")] pub BTreeMap<ExecKey, Verdict>);

/// A key written twice meets.
impl FromIterator<(ExecKey, Verdict)> for ExecGrant {
    fn from_iter<I: IntoIterator<Item = (ExecKey, Verdict)>>(keys: I) -> Self {
        let mut grant = Self::default();
        for (key, v) in keys {
            meet_insert(&mut grant.0, key, v);
        }
        grant
    }
}

/// The one way an exec entry is added: a key already present meets.
pub(crate) fn meet_insert<K: Ord, V: Meet>(map: &mut BTreeMap<K, V>, key: K, value: V) {
    let value = match map.remove(&key) {
        Some(held) => held.meet(value),
        None => value,
    };
    map.insert(key, value);
}

/// A map keyed by a struct, as the sequence of its pairs — no JSON object can
/// key one — read back through [`meet_insert`].
mod pairs {
    use super::{Meet, meet_insert};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub(super) fn serialize<K: Serialize, V: Serialize, S: Serializer>(
        map: &BTreeMap<K, V>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(map)
    }

    pub(super) fn deserialize<'de, K, V, D>(deserializer: D) -> Result<BTreeMap<K, V>, D::Error>
    where
        K: Deserialize<'de> + Ord,
        V: Deserialize<'de> + Meet,
        D: Deserializer<'de>,
    {
        let mut map = BTreeMap::new();
        for (key, value) in Vec::<(K, V)>::deserialize(deserializer)? {
            meet_insert(&mut map, key, value);
        }
        Ok(map)
    }
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
/// [`NormalizedPrefix`]es carry the `resolved` form the meet keys on.  Nothing
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
#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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

/// Exec rules for the kernel, in order; the last rule that matches decides.
///
/// Under `Unrestricted` the in-process guard is the only check; `Restricted`
/// closes the OS layer around the same table, shutting the `sh -c
/// "PATH=…; cmd"` route by which a sandboxed child re-execs binaries the
/// guard never sees.  An empty `Restricted` admits nothing.
///
/// Paths are [`RealPath`]s, frozen at grant, and stay so until the backend
/// that needs other spellings renders them with [`render_real`], which never
/// re-resolves one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecProjection<N = RealPath> {
    Unrestricted,
    Restricted(Vec<ExecRule<N>>),
}

/// One kernel exec rule.  A `Veto` stays `String` while paths are `N`: a bare
/// name is not a path, so rendering it stops typechecking rather than
/// emitting a rule for `/git`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecRule<N = RealPath> {
    Dir { path: N, allow: bool },
    File { path: N, allow: bool },
    Veto(String),
}

impl<N> ExecRule<N> {
    /// Each name `f` renders becomes a rule with this one's `allow`, in place.
    #[cfg(target_os = "macos")]
    pub(crate) fn try_flat_map<M, E>(
        &self,
        mut f: impl FnMut(&N) -> Result<Vec<M>, E>,
    ) -> Result<Vec<ExecRule<M>>, E> {
        Ok(match self {
            Self::Dir { path, allow } => f(path)?
                .into_iter()
                .map(|path| ExecRule::Dir {
                    path,
                    allow: *allow,
                })
                .collect(),
            Self::File { path, allow } => f(path)?
                .into_iter()
                .map(|path| ExecRule::File {
                    path,
                    allow: *allow,
                })
                .collect(),
            Self::Veto(name) => vec![ExecRule::Veto(name.clone())],
        })
    }
}

/// Hand-written for the same reason as [`FsProjection`]'s.
impl<N> Default for ExecProjection<N> {
    fn default() -> Self {
        Self::Unrestricted
    }
}

impl<N> ExecProjection<N> {
    /// Whether some rule denies rather than merely not admitting — the
    /// distinction between an allow-set that is narrow and one that is
    /// narrowed on purpose, which is what a backend must protect.
    #[cfg(target_os = "macos")]
    pub(crate) fn carries_veto(&self) -> bool {
        match self {
            Self::Unrestricted => false,
            Self::Restricted(rules) => rules.iter().any(|rule| {
                matches!(
                    rule,
                    ExecRule::Veto(_)
                        | ExecRule::Dir { allow: false, .. }
                        | ExecRule::File { allow: false, .. }
                )
            }),
        }
    }
}

/// The OS-renderable projection of the effective grant, produced by
/// `sandbox_projection` in `core/src/capability/sandbox.rs` after meet-folding
/// the whole stack.
///
/// The platform backends `sandbox::linux` and `sandbox::macos` render it, in
/// the launching process. Unlike a [`Capabilities`] frame, no further
/// composition can widen it.
///
/// `N` is how the projection *names* the objects its fs rules rule over;
/// exec rules name real paths throughout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxProjection<N = String> {
    pub fs: FsProjection<N>,
    pub net: bool,
    pub exec: ExecProjection,
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
    /// [`render_real`].  A path set added later therefore fails to compile until it
    /// too is threaded — which is the point, since every under-enforcement of
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
}

/// Gates the `_ed-*` builtins.
///
/// `deny_unknown_fields` is structural: TOML attaches every key after a
/// header to that header, so a stray top-level key drifting into `[editor]`
/// must error rather than be silently dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorPolicy {
    pub(crate) read: bool,
    pub(crate) write: bool,
    pub(crate) tui: bool,
}

/// Gates the `cd` builtin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellPolicy {
    pub chdir: bool,
}

/// One layer of the dynamic grant stack — the per-effect policies, with every
/// `~` / `xdg:` / `cwd:` / `tempdir:` sigil already resolved to a concrete
/// path.
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
    pub exec: Option<ExecGrant>,
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

    /// A stack of exactly `frame`: a view for asking the in-process guard's question of one
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

    pub fn push(&mut self, layer: Capabilities) {
        self.0.push(layer);
    }

    /// Remove the `n` frames pushed at `at`, keeping any session frame pushed
    /// above them meanwhile.
    pub(crate) fn remove(&mut self, at: usize, n: usize) {
        self.0.drain(at..at + n);
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Capabilities> {
        self.0.iter()
    }

    /// Layers with no opinion are skipped, here and in `fs` and `net` below:
    /// the folds intersect what remains, and an empty run means no attenuation.
    pub fn exec(&self) -> impl Iterator<Item = &ExecGrant> {
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
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn permits_detach(&self) -> bool {
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

impl IntoIterator for GrantStack {
    type Item = Capabilities;
    type IntoIter = std::vec::IntoIter<Capabilities>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl Capabilities {
    /// Lattice bottom for positive authority: every effect pinned to its
    /// most-restrictive value, so a `meet` against it zeroes every dimension.
    pub fn deny_all() -> Self {
        Self {
            exec: Some(ExecGrant::default()),
            fs: Some(FsPolicy::default()),
            net: Some(false),
            detach: Some(false),
            editor: Some(EditorPolicy::default()),
            shell: Some(ShellPolicy::default()),
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
    /// Widen `self` with `other` — the composition `--extend-base` runs to lift
    /// a base ceiling before any attenuation.  Positive authority unions, but
    /// every veto survives from either side, so an extension can grant where
    /// the base was silent and never re-admit what it denied.  Hence no
    /// order-dual of [`Meet`]: a deny is a floor under both.
    pub fn widen(self, other: Self) -> Self {
        Self {
            exec: self.exec.widen(other.exec),
            fs: self.fs.widen(other.fs),
            net: self.net.widen(other.net),
            detach: self.detach.widen(other.detach),
            editor: self.editor.widen(other.editor),
            shell: self.shell.widen(other.shell),
        }
    }
}

// ── Lattice impls ─────────────────────────────────────────────────────────

impl Verdict {
    /// `name`, or `name[sub1,sub2,…]` under `Only`; `None` when denied.
    /// The set iterates sorted, so the label is deterministic.
    pub fn admit_label(&self, name: &str) -> Option<String> {
        match self {
            Self::Allow => Some(name.to_string()),
            Self::Only(subs) => Some(format!(
                "{name}[{}]",
                subs.iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            )),
            Self::Deny => None,
        }
    }
}

impl Verdict {
    pub fn is_denied(&self) -> bool {
        matches!(self, Self::Deny)
    }
}

impl From<bool> for Verdict {
    fn from(allow: bool) -> Self {
        if allow { Self::Allow } else { Self::Deny }
    }
}

impl Meet for Verdict {
    fn meet(self, other: Self) -> Self {
        match (self, other) {
            (Self::Deny, _) | (_, Self::Deny) => Self::Deny,
            (Self::Allow, v) | (v, Self::Allow) => v,
            (Self::Only(a), Self::Only(b)) => Self::Only(&a & &b),
        }
    }
}

impl Widen for Verdict {
    fn widen(self, other: Self) -> Self {
        match (self, other) {
            // Deny-overrides under widening too, so `--extend-base` can never
            // re-admit a command the base denies.
            (Self::Deny, _) | (_, Self::Deny) => Self::Deny,
            (Self::Allow, _) | (_, Self::Allow) => Self::Allow,
            (Self::Only(a), Self::Only(b)) => Self::Only(&a | &b),
        }
    }
}

impl Widen for FsPolicy {
    fn widen(self, other: Self) -> Self {
        Self {
            read_prefixes: union_prefixes(self.read_prefixes, other.read_prefixes),
            write_prefixes: union_prefixes(self.write_prefixes, other.write_prefixes),
            // Denies union exactly as in `meet`: an overlay silent on a base
            // carve-out must not lift it.
            deny_paths: union_prefixes(self.deny_paths, other.deny_paths),
        }
    }
}

impl Widen for EditorPolicy {
    fn widen(self, other: Self) -> Self {
        Self {
            read: self.read.widen(other.read),
            write: self.write.widen(other.write),
            tui: self.tui.widen(other.tui),
        }
    }
}

impl Widen for ShellPolicy {
    fn widen(self, other: Self) -> Self {
        Self {
            chdir: self.chdir.widen(other.chdir),
        }
    }
}

/// Keys widen one by one, then an
/// [`evicts`](NormalizedPrefix::evicts) sweep drops any allow dir that clashes
/// with a deny dir — whatever spelling either side used — so an overlay that
/// re-grants a directory the base vetoed still loses it.
impl Widen for ExecGrant {
    fn widen(self, other: Self) -> Self {
        let mut keys = widen_keys(self.0, other.0);
        let denied: Vec<NormalizedPrefix> = (keys.iter())
            .filter_map(|(key, v)| match key {
                ExecKey::Dir(dir) if v.is_denied() => Some(dir.clone()),
                _ => None,
            })
            .collect();
        keys.retain(|key, v| match key {
            ExecKey::Dir(dir) => v.is_denied() || !denied.iter().any(|d| d.evicts(dir)),
            _ => true,
        });
        Self(keys)
    }
}

/// Shared keys widen, and a one-sided key survives verbatim: an absent key is
/// the widening identity, so silence on one side lifts neither the other's
/// grant nor its veto.
fn widen_keys<K: Ord, V: Widen>(mut a: BTreeMap<K, V>, b: BTreeMap<K, V>) -> BTreeMap<K, V> {
    for (key, v) in b {
        let v = match a.remove(&key) {
            Some(held) => held.widen(v),
            None => v,
        };
        a.insert(key, v);
    }
    a
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
mod rendered_tests {
    use super::*;

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
    #[cfg(target_os = "macos")]
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
    #[cfg(target_os = "macos")]
    fn reach_alone(write: &[&str], dir: &str) -> WriteReach {
        reach(write, &[dir], dir)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_write_prefix_identical_to_the_admit_is_trusted() {
        assert_eq!(
            reach_alone(&["/ral-test/w"], "/ral-test/w"),
            WriteReach::Trusted
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_write_prefix_inside_the_admit_is_trusted() {
        assert_eq!(
            reach_alone(&["/ral-test/w/out"], "/ral-test/w"),
            WriteReach::Trusted
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_shallower_write_prefix_covers_the_admit() {
        assert_eq!(
            reach_alone(&["/ral-test"], "/ral-test/w/bin"),
            WriteReach::Covered
        );
        assert_eq!(reach_alone(&["/"], "/ral-test/w/bin"), WriteReach::Covered);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_disjoint_write_prefix_leaves_the_admit_apart() {
        assert_eq!(
            reach_alone(&["/ral-test/other"], "/ral-test/w"),
            WriteReach::Apart
        );
        assert_eq!(reach_alone(&[], "/ral-test/w"), WriteReach::Apart);
    }

    /// Containment is by component: `/ral-test/w2` is no part of `/ral-test/w`.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_sibling_that_shares_a_name_prefix_is_apart() {
        assert_eq!(
            reach_alone(&["/ral-test/w2"], "/ral-test/w"),
            WriteReach::Apart
        );
    }

    /// Naming the tree outranks a shallower prefix that also reaches it.
    #[cfg(target_os = "macos")]
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
    #[cfg(target_os = "macos")]
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
    #[cfg(target_os = "macos")]
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
    #[cfg(target_os = "macos")]
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
