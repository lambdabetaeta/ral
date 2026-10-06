//! The capability lattice: one frame of typed authority, and the folds over it.
//!
//! A [`Capabilities`] frame bundles the per-effect policies.  Composition
//! downward is the [`GrantStack`]: a `grant { ... }` or a loaded profile
//! pushes one more layer, and every verdict folds the layers afresh,
//! `exec::rules`, `fs::region`, [`GrantStack::permits`], rather than
//! flattening them into one `Capabilities` first.
//! [`Capabilities::widen`] is the one place a frame still composes eagerly:
//! `--extend-base` widens a base ceiling at load time, before any attenuation
//! runs, and its "silence lifts no veto" semantics only make sense as a
//! single-layer union.  Denies are sticky under both regimes.
//!
//! This is data, with no reader of its own: the in-process guard
//! (`crate::guard`) and the OS sandbox (`crate::sandbox`) are its two
//! consumers.

use crate::path::FrozenPath;
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

/// An exec key as the author wrote it: the written twin of
/// `capability::exec::ExecScope`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ExecKey {
    /// A bare name: `git`.
    Name(String),
    /// A path, frozen: `/usr/bin/git`, `~/bin/x`.
    Path(FrozenPath),
    /// A directory, frozen, and the expansions of `path:` and `system:`.
    Dir(FrozenPath),
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
    pub read_prefixes: Vec<FrozenPath>,
    #[serde(default)]
    pub write_prefixes: Vec<FrozenPath>,
    #[serde(default)]
    pub deny_paths: Vec<FrozenPath>,
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
/// `guard::decode::decode_capability_map`, which resolves every sigil against
/// a `FreezeCtx` before returning, and the remaining ways in ([`root`](Self::root), [`deny_all`](Self::deny_all),
/// `default`) are path-free.  So the `Serialize` impls can back the seed's
/// `Context` unguarded: a re-exec'd child inherits a stack whose paths
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

/// A boolean authority, gating a verb or a builtin rather than an access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    Detach,
    EditorRead,
    EditorWrite,
    EditorTui,
    ShellChdir,
}

impl Flag {
    /// What a layer votes; `None` abstains.
    fn vote(self, caps: &Capabilities) -> Option<bool> {
        match self {
            Self::Detach => caps.detach,
            Self::EditorRead => caps.editor.as_ref().map(|e| e.read),
            Self::EditorWrite => caps.editor.as_ref().map(|e| e.write),
            Self::EditorTui => caps.editor.as_ref().map(|e| e.tui),
            Self::ShellChdir => caps.shell.as_ref().map(|s| s.chdir),
        }
    }

    /// The key as a grant spells it.
    pub fn key(self) -> &'static str {
        match self {
            Self::Detach => "detach",
            Self::EditorRead => "editor.read",
            Self::EditorWrite => "editor.write",
            Self::EditorTui => "editor.tui",
            Self::ShellChdir => "shell.chdir",
        }
    }
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

    /// A meet over the layers: one `false` vote anywhere withholds `flag`
    /// whatever sits above it, and silence permits.  A verb's gate, not an OS
    /// rule, so it reaches no sandbox projection.
    pub fn permits(&self, flag: Flag) -> bool {
        self.0.iter().all(|c| flag.vote(c) != Some(false))
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
/// [`evicts`](FrozenPath::evicts) sweep drops any allow dir that clashes
/// with a deny dir — whatever spelling either side used — so an overlay that
/// re-grants a directory the base vetoed still loses it.
impl Widen for ExecGrant {
    fn widen(self, other: Self) -> Self {
        let mut keys = widen_keys(self.0, other.0);
        let denied: Vec<FrozenPath> = (keys.iter())
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

fn union_prefixes(a: Vec<FrozenPath>, b: Vec<FrozenPath>) -> Vec<FrozenPath> {
    a.into_iter()
        .chain(b)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
