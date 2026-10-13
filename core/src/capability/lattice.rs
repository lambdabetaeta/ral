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
use crate::syntax::quote::quote_word;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use strum::{EnumString, IntoStaticStr, VariantArray};

// ── Lattice traits ────────────────────────────────────────────────────────
//
// The two semilattice operations, beside the types they run over.  The
// `Option<T>` lift rides along because `None` as identity — a layer with no
// opinion on a field — is a capability convention, not a universal one.

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

/// `Deny < Allow`.
///
/// A `Deny` is sticky under meet *and* widen, even against a layer whose grant
/// omits the key, so a base can pin a command out once and no overlay
/// re-grants it; only a different base lifts one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    Allow,
    Deny,
}

/// An exec key as the author wrote it: the written twin of
/// `capability::exec::ExecScope`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ExecKey {
    /// A bare name: `git`.
    Name(String),
    /// A path, frozen: `/usr/bin/git`.
    Path(FrozenPath),
    /// A directory, frozen, and each directory a selector expands to.
    Dir(FrozenPath),
}

/// The key as a grant spells it: a file as an `absolute` place, a directory
/// under its `dir` constructor.
impl std::fmt::Display for ExecKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Name(name) => write!(f, "`name {}", quote_word(name)),
            Self::Path(path) => write!(f, "`absolute {}", quote_word(path.as_str())),
            Self::Dir(dir) => write!(f, "`dir `absolute {}", quote_word(dir.as_str())),
        }
    }
}

/// Exec authority as authored, per layer.
///
/// The author's spellings survive for display; `capability::exec` compiles
/// the grant into rules over programs, and nothing matches it directly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecGrant(#[serde(with = "pairs")] BTreeMap<ExecKey, Verdict>);

impl ExecGrant {
    pub fn iter(&self) -> impl Iterator<Item = (&ExecKey, &Verdict)> {
        self.0.iter()
    }

    pub fn get(&self, key: &ExecKey) -> Option<&Verdict> {
        self.0.get(key)
    }

    /// Prunes entries; rewrites none.
    pub fn retain(&mut self, mut keep: impl FnMut(&ExecKey, &Verdict) -> bool) {
        self.0.retain(|key, v| keep(key, v));
    }
}

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
/// credential dirs (`xdg-config gh`, `xdg-config op`, …) unreadable under a
/// wholesale-readable `xdg-config .`.
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

/// The `_ed-*` builtins a layer lets through.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    VariantArray,
    EnumString,
    IntoStaticStr,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum EditorVerb {
    Read,
    Write,
    Tui,
}

/// The shell verbs a layer lets through: `cd`, and birthing a process this
/// session stops owning.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    VariantArray,
    EnumString,
    IntoStaticStr,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum ShellVerb {
    Chdir,
    Detach,
}

/// Networking a layer withholds; the veto is sticky.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    VariantArray,
    EnumString,
    IntoStaticStr,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum Net {
    Offline,
}

/// One layer of the dynamic grant stack — the per-effect policies, with every
/// place already resolved to a concrete path.
///
/// Resolved *by construction*: the only non-trivial constructor is
/// `guard::decode::decode_capability_map`, which resolves every place against
/// a `FreezeCtx` before returning, and the remaining ways in ([`root`](Self::root), [`deny_all`](Self::deny_all),
/// `default`) are path-free.  So the `Serialize` impls can back the seed's
/// `Context` unguarded: a re-exec'd child inherits a stack whose paths
/// the parent already pinned.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    #[serde(default)]
    pub exec: Option<ExecGrant>,
    #[serde(default)]
    pub fs: Option<FsPolicy>,
    #[serde(default)]
    pub net: Option<Net>,
    /// `None` inherits, so silence permits as on every other axis.
    #[serde(default)]
    pub editor: Option<BTreeSet<EditorVerb>>,
    #[serde(default)]
    pub shell: Option<BTreeSet<ShellVerb>>,
}

/// A verb a layer may withhold, gating a builtin rather than an access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    Editor(EditorVerb),
    Shell(ShellVerb),
}

impl Flag {
    /// Whether a layer lists the verb; `None` abstains.
    fn vote(self, caps: &Capabilities) -> Option<bool> {
        match self {
            Self::Editor(v) => caps.editor.as_ref().map(|s| s.contains(&v)),
            Self::Shell(v) => caps.shell.as_ref().map(|s| s.contains(&v)),
        }
    }

    /// The key as a grant spells it, for example `` shell: [`detach] ``.
    pub fn key(self) -> String {
        match self {
            Self::Editor(v) => format!("editor: [`{}]", <&str>::from(v)),
            Self::Shell(v) => format!("shell: [`{}]", <&str>::from(v)),
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

    pub fn net(&self) -> impl Iterator<Item = Net> {
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
            net: Some(Net::Offline),
            editor: Some(BTreeSet::new()),
            shell: Some(BTreeSet::new()),
        }
    }

    /// True iff some supplied axis restricts: exec and fs whenever supplied,
    /// net when offline, a verb set unless it lists every verb.
    pub fn is_restrictive(&self) -> bool {
        self.exec.is_some()
            || self.fs.is_some()
            || self.net == Some(Net::Offline)
            || withholds(self.editor.as_ref())
            || withholds(self.shell.as_ref())
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
            // An omitted base axis is every verb, which nothing widens.
            editor: self.editor.map(|b| &b | &other.editor.unwrap_or_default()),
            shell: self.shell.map(|b| &b | &other.shell.unwrap_or_default()),
        }
    }
}

// ── Lattice impls ─────────────────────────────────────────────────────────

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
            (Self::Allow, Self::Allow) => Self::Allow,
        }
    }
}

impl Widen for Verdict {
    fn widen(self, other: Self) -> Self {
        match (self, other) {
            // Deny-overrides under widening too, so `--extend-base` can never
            // re-admit a command the base denies.
            (Self::Deny, _) | (_, Self::Deny) => Self::Deny,
            (Self::Allow, Self::Allow) => Self::Allow,
        }
    }
}

impl Widen for Net {
    fn widen(self, _: Self) -> Self {
        Self::Offline
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

/// A verb set that lists fewer than every verb.
fn withholds<V: VariantArray>(verbs: Option<&BTreeSet<V>>) -> bool {
    verbs.is_some_and(|s| s.len() < V::VARIANTS.len())
}
