//! The capability lattice: one frame of typed authority, and the folds over it.
//!
//! A [`Capabilities`] frame bundles the per-effect policies.  Composition
//! downward is the [`GrantStack`]: a `grant { ... }` or a loaded profile
//! pushes one more layer, and every verdict folds the layers afresh —
//! `capability::exec::rules`, `allow_region`/`deny_region`, `permits_detach` —
//! rather than flattening them into one `Capabilities` first.
//! [`Capabilities::widen`] is the one place a frame still composes eagerly:
//! `--extend-base` widens a base ceiling at load time, before any attenuation
//! runs, and its "silence lifts no veto" semantics only make sense as a
//! single-layer union.  Denies are sticky under both regimes.
//!
//! [`SandboxProjection`] is the meet-folded fs+net+exec residue the OS sandbox
//! backends render; `detach` gates a verb instead of an OS rule, so it is folded
//! at the call by [`GrantStack::permits_detach`] and reaches no projection.

use crate::path::{NormalizedPrefix, Rendered, render_paths, rendered_pins};
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

/// Exec authority as authored, per layer.
///
/// Bare names, path keys and directory keys, each with its verdict.  The
/// author's spellings survive for display; `capability::exec` compiles the
/// grant into rules over programs, and nothing matches it directly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecGrant {
    /// Bare keys: `git`.
    #[serde(default)]
    pub names: BTreeMap<String, Verdict>,
    /// Path keys, frozen: `/usr/bin/git`, `~/bin/x`.
    #[serde(default, with = "pairs")]
    pub paths: BTreeMap<NormalizedPrefix, Verdict>,
    /// Dir keys, and the expansions of `path:` and `system:`; `true` admits.
    #[serde(default, with = "pairs")]
    pub dirs: BTreeMap<NormalizedPrefix, bool>,
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
    /// The ancestor closure of every *rendered* `deny_paths` entry that lies
    /// within some *rendered* write name.  The macOS backend pins each
    /// against rename and unlink, or a confined child relocates the ancestor
    /// directory itself (`mv /repo/.ssh /repo/x`, or `mv /repo /scratch/r`
    /// when the write prefix root is the ancestor) and the denied bytes
    /// resurface at a name no deny rule covers.  Rendering runs before the
    /// ancestor walk (`SandboxProjection::traverse`), so both a deny's
    /// surface chain and its resolved chain are pinned, and an alias
    /// `W/alias → W/top/deep` with deny `W/alias/secret` pins `W/top` too —
    /// an ancestor chain only the resolved spelling reveals.
    #[serde(skip)]
    pub(crate) pinned_dirs: Vec<N>,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "rules", rename_all = "snake_case")]
pub enum ExecProjection<N = String> {
    Unrestricted,
    Restricted(Vec<ExecRule<N>>),
}

/// One kernel exec rule.  A `Veto` stays `String` while paths are `N`: a bare
/// name is not a path, so rendering it stops typechecking rather than
/// emitting a rule for `/git`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecRule<N = String> {
    Dir { path: N, allow: bool },
    File { path: N, allow: bool },
    Veto(String),
}

impl<N> ExecRule<N> {
    /// Each name `f` renders becomes a rule with this one's `allow`, in place.
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
    /// each fs set once on the way and deriving `pinned_dirs` from the result.
    ///
    /// The completeness guarantee is structural, not promised: the input is
    /// destructured exhaustively, the output constructed exhaustively, and the
    /// only way to obtain a `Vec<Rendered>` or an `ExecRule<Rendered>` is to
    /// call `f`.  A path set added
    /// later therefore fails to compile until it too is threaded — which is
    /// the point, since every under-enforcement of this class has been someone
    /// forgetting to expand one new list.
    ///
    /// Pins are derived here rather than carried because they are a *function*
    /// of the deny paths and the write prefixes — but only once both are
    /// rendered: containment before expansion and after do not commute, since
    /// a name reached only through a symlinked chain has ancestors the
    /// surface spelling never mentions.  So `f` runs on the deny and write
    /// sets first, and the ancestor walk runs on their rendered output.
    ///
    /// # Errors
    ///
    /// Whatever `f` refuses; [`render_paths`] refuses a name whose expansion
    /// it cannot spell faithfully.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn traverse(
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
                let write_prefixes = f(&ordered(write_prefixes))?;
                let deny_paths = f(&ordered(deny_paths))?;
                let pinned_dirs = rendered_pins(&deny_paths, &write_prefixes);
                FsProjection::Restricted(FsRules {
                    read_prefixes: f(&ordered(read_prefixes))?,
                    write_prefixes,
                    deny_paths,
                    pinned_dirs,
                })
            }
        };
        let exec = match exec {
            ExecProjection::Unrestricted => ExecProjection::Unrestricted,
            // Order is precedence here, so the rules are neither sorted nor deduped.
            ExecProjection::Restricted(rules) => {
                let mut rendered = Vec::with_capacity(rules.len());
                for rule in rules {
                    rendered.extend(rule.try_flat_map(|path| f(std::slice::from_ref(path)))?);
                }
                ExecProjection::Restricted(rendered)
            }
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
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn rendered(&self) -> Result<SandboxProjection<Rendered>, String> {
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

/// Names, paths and dirs widen key by key, then an
/// [`evicts`](NormalizedPrefix::evicts) sweep drops any allow dir that clashes
/// with a deny dir — whatever spelling either side used — so an overlay that
/// re-grants a directory the base vetoed still loses it.
impl Widen for ExecGrant {
    fn widen(self, other: Self) -> Self {
        let mut dirs = widen_keys(self.dirs, other.dirs);
        let denied: Vec<NormalizedPrefix> = dirs
            .iter()
            .filter(|(_, allow)| !**allow)
            .map(|(dir, _)| dir.clone())
            .collect();
        dirs.retain(|dir, allow| !*allow || !denied.iter().any(|d| d.evicts(dir)));
        Self {
            names: widen_keys(self.names, other.names),
            paths: widen_keys(self.paths, other.paths),
            dirs,
        }
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
mod traverse_tests {
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

    /// Bare names are not paths: a veto passes the traversal untouched,
    /// while each rendered spelling of a path keeps its rule's place.
    #[test]
    fn a_veto_is_carried_across_unexpanded_and_paths_keep_their_place() {
        let out = SandboxProjection {
            exec: ExecProjection::Restricted(vec![
                ExecRule::Dir {
                    path: "/usr/bin".to_string(),
                    allow: true,
                },
                ExecRule::Veto("git".to_string()),
                ExecRule::File {
                    path: "/usr/bin/git".to_string(),
                    allow: false,
                },
            ]),
            ..SandboxProjection::default()
        }
        .rendered()
        .expect("ASCII paths render");
        let ExecProjection::Restricted(rules) = out.exec else {
            panic!("restricted in, restricted out");
        };
        let veto = rules
            .iter()
            .position(|r| matches!(r, ExecRule::Veto(name) if name == "git"))
            .expect("the veto survives");
        assert!(rules[..veto].iter().all(
            |r| matches!(r, ExecRule::Dir { path, allow: true } if path.as_str().ends_with("/usr/bin"))
        ));
        assert!(rules[veto + 1..].iter().all(
            |r| matches!(r, ExecRule::File { path, allow: false } if path.as_str().ends_with("/usr/bin/git"))
        ));
    }
}
