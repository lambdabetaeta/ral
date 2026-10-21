//! The capability lattice: one frame of typed authority, and the folds over it.
//!
//! A [`Capabilities`] frame bundles the per-effect policies.  Composition
//! downward is the [`GrantStack`]: a `grant { ... }` or a loaded profile
//! pushes one more layer, and every judgment folds the layers afresh,
//! `exec::rules`, `fs::region`, [`GrantStack::permits`], rather than
//! flattening them into one `Capabilities` first.
//! [`Capabilities::extend`] is the one place a frame still composes eagerly:
//! `--extend` joins a [`Fragment`] onto a base ceiling at load time, before
//! any attenuation runs.
//!
//! Every policy denotes a function from requests to Belnap's four values,
//! [`Evidence`].  Restriction is truth meet ([`Meet`]), extension information
//! join ([`Join`]); an omitted axis is `t` in a policy and `⊥` in a fragment,
//! and the two roles are two types.  Only the enforcing boundary closes
//! evidence to a decision ([`Evidence::permits`]).
//!
//! This is data, with no reader of its own: the in-process guard
//! (`crate::guard`) and the OS sandbox (`crate::sandbox`) are its two
//! consumers.

use crate::path::FrozenPath;
use crate::syntax::quote::quote_word;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use strum::{EnumString, IntoStaticStr, VariantArray};

// ── Evidence and its operators ────────────────────────────────────────────

/// Belnap's FOUR as the product bilattice over the Booleans: positive and
/// negative support for one request.
///
/// Neither is not a deny; Both is not malformed: allowing a directory and
/// excluding a subtree produces Both inside the subtree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub positive: bool,
    pub negative: bool,
}

impl Evidence {
    pub const NEITHER: Self = Self {
        positive: false,
        negative: false,
    };
    pub const ALLOW: Self = Self {
        positive: true,
        negative: false,
    };
    pub const DENY: Self = Self {
        positive: false,
        negative: true,
    };
    pub const BOTH: Self = Self {
        positive: true,
        negative: true,
    };

    /// Belnap's `↓`, applied once, where a decision is enforced.
    pub fn permits(self) -> bool {
        self.positive && !self.negative
    }
}

/// `∧`: composes restricting layers; positive support must agree, negative
/// survives.  Identity `t` (ALLOW everywhere).
pub trait Meet {
    fn meet(self, other: Self) -> Self;
}

/// `⊕`: collects the evidence of two supplied fragments.  Identity `⊥`.
pub trait Join {
    fn join(self, other: Self) -> Self;
}

impl Meet for Evidence {
    fn meet(self, other: Self) -> Self {
        Self {
            positive: self.positive && other.positive,
            negative: self.negative || other.negative,
        }
    }
}

impl Join for Evidence {
    fn join(self, other: Self) -> Self {
        Self {
            positive: self.positive || other.positive,
            negative: self.negative || other.negative,
        }
    }
}

/// An authored axis policy, as a fragment over the unrestricted policy.
pub trait Axis: Join + Sized {
    /// `t ⊕ self`; `None`, which is `t`, where omission loses nothing.
    fn join_top(self) -> Option<Self>;
    /// Denotes `t`: restricts nothing.
    fn is_top(&self) -> bool;
}

/// An extension fragment: an omitted axis states nothing (`⊥`), where an
/// omitted axis of a [`Capabilities`] restricts nothing (`t`).  Only
/// [`Capabilities::extend`] consumes one, and only the loader makes one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment(Capabilities);

impl Fragment {
    /// Only `load::profile` calls this.
    pub(crate) fn of(capabilities: Capabilities) -> Self {
        Self(capabilities)
    }
}

/// `base ◁ fragment` on one axis.
fn extend<T: Axis>(base: Option<T>, fragment: Option<T>) -> Option<T> {
    match (base, fragment) {
        (base, None) => base,
        (None, Some(fragment)) => fragment.join_top(),
        (Some(base), Some(fragment)) => Some(base.join(fragment)),
    }
}

/// Add `evidence` at `key`, joining what is held.  The one way evidence enters
/// a map; Neither is never stored.
pub(crate) fn join_insert<K: Ord>(map: &mut BTreeMap<K, Evidence>, key: K, evidence: Evidence) {
    if evidence != Evidence::NEITHER {
        map.entry(key)
            .and_modify(|held| *held = held.join(evidence))
            .or_insert(evidence);
    }
}

/// An exec key as the author wrote it: the written twin of
/// `capability::exec::{ExecAllow, ExecDeny}`.
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
            Self::Path(path) => write!(f, "`absolute {}", quote_word(&path.shown())),
            Self::Dir(dir) => write!(f, "`dir `absolute {}", quote_word(&dir.shown())),
        }
    }
}

/// Exec authority as authored, per layer.
///
/// The author's spellings survive for display; `capability::exec` compiles
/// the grant into rules over programs, and nothing matches it directly.  Both
/// fields are private: entries come only through joining, so no grant holds
/// Neither, and `every` only through [`Axis::join_top`], so "every program"
/// has no negative form.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecGrant {
    /// `join_top`'s universal support: every program, never authored.
    every: bool,
    #[serde(with = "pairs")]
    rules: BTreeMap<ExecKey, Evidence>,
}

impl ExecGrant {
    pub fn every(&self) -> bool {
        self.every
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ExecKey, &Evidence)> {
        self.rules.iter()
    }

    pub fn get(&self, key: &ExecKey) -> Option<&Evidence> {
        self.rules.get(key)
    }

    /// Withdraws the allowances `drop` selects; an exclusion stays.
    pub fn withdraw(&mut self, mut drop: impl FnMut(&ExecKey) -> bool) {
        for (key, e) in &mut self.rules {
            if drop(key) {
                e.positive = false;
            }
        }
        self.rules.retain(|_, e| *e != Evidence::NEITHER);
    }

    fn denies(&self) -> bool {
        self.rules.values().any(|e| e.negative)
    }
}

/// A key written twice joins.
impl FromIterator<(ExecKey, Evidence)> for ExecGrant {
    fn from_iter<I: IntoIterator<Item = (ExecKey, Evidence)>>(keys: I) -> Self {
        let mut rules = BTreeMap::new();
        for (key, e) in keys {
            join_insert(&mut rules, key, e);
        }
        Self {
            every: false,
            rules,
        }
    }
}

/// A map keyed by a struct, as the sequence of its pairs (no JSON object can
/// key one), read back through [`join_insert`]: a repeated key or a Neither
/// value is not what the encoder emits, so the other end is not this tree.
mod pairs {
    use super::{Evidence, join_insert};
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub(super) fn serialize<K: Serialize, S: Serializer>(
        map: &BTreeMap<K, Evidence>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(map)
    }

    pub(super) fn deserialize<'de, K, D>(deserializer: D) -> Result<BTreeMap<K, Evidence>, D::Error>
    where
        K: Deserialize<'de> + Ord,
        D: Deserializer<'de>,
    {
        let mut map = BTreeMap::new();
        for (key, e) in Vec::<(K, Evidence)>::deserialize(deserializer)? {
            if e == Evidence::NEITHER {
                return Err(D::Error::custom("a rule holds no evidence"));
            }
            let held = map.len();
            join_insert(&mut map, key, e);
            if map.len() == held {
                return Err(D::Error::custom("a rule key repeats"));
            }
        }
        Ok(map)
    }
}

impl Join for ExecGrant {
    fn join(mut self, other: Self) -> Self {
        self.every |= other.every;
        for (key, e) in other.rules {
            join_insert(&mut self.rules, key, e);
        }
        self
    }
}

impl Axis for ExecGrant {
    /// The entries stay beside `every`: they change nothing about running,
    /// but the stack still reads what they name.
    fn join_top(self) -> Option<Self> {
        (self.every || !self.rules.is_empty()).then_some(Self {
            every: true,
            ..self
        })
    }

    fn is_top(&self) -> bool {
        self.every && !self.denies()
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
    /// Decision-identical to `[root, frame]` because the ambient root holds no
    /// opinion on any axis.
    pub fn of(frame: Capabilities) -> Self {
        Self(vec![frame])
    }

    /// True iff some layer restricts (denotes other than `t`) — what
    /// `Shell::has_active_capabilities` reports.
    pub fn is_restrictive(&self) -> bool {
        self.0.iter().any(Capabilities::is_restrictive)
    }

    /// Whether no layer restricts exec: the one question of exec denoting `t`.
    pub fn exec_is_top(&self) -> bool {
        self.exec().all(Axis::is_top)
    }

    /// Whether no layer restricts fs: the one question of fs denoting `t`.
    pub fn fs_is_top(&self) -> bool {
        self.fs().all(Axis::is_top)
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
    /// most-restrictive value, so pushing it as a layer zeroes every axis.
    pub fn deny_all() -> Self {
        Self {
            exec: Some(ExecGrant::default()),
            fs: Some(FsPolicy::default()),
            net: Some(Net::Offline),
            editor: Some(BTreeSet::new()),
            shell: Some(BTreeSet::new()),
        }
    }

    /// True iff some supplied axis does not denote `t`.
    pub fn is_restrictive(&self) -> bool {
        fn restricts<T: Axis>(axis: Option<&T>) -> bool {
            axis.is_some_and(|a| !a.is_top())
        }
        restricts(self.exec.as_ref())
            || restricts(self.fs.as_ref())
            || restricts(self.net.as_ref())
            || restricts(self.editor.as_ref())
            || restricts(self.shell.as_ref())
    }

    /// Ambient authority, the lattice top: all fields `None`, no attenuation.
    pub fn root() -> Self {
        Self::default()
    }

    /// `self ◁ fragment`: the ceiling a base and an extension make, before any
    /// restriction.  An exclusion of the fragment always wins; an allowance
    /// never removes authority; an axis the base omits stays unrestricted,
    /// less the fragment's exclusions.
    pub fn extend(self, fragment: Fragment) -> Self {
        let Fragment(fragment) = fragment;
        Self {
            exec: extend(self.exec, fragment.exec),
            fs: extend(self.fs, fragment.fs),
            net: extend(self.net, fragment.net),
            editor: extend(self.editor, fragment.editor),
            shell: extend(self.shell, fragment.shell),
        }
    }
}

// ── Axis impls ────────────────────────────────────────────────────────────

impl Join for Net {
    fn join(self, _: Self) -> Self {
        Self::Offline
    }
}

impl Axis for Net {
    fn join_top(self) -> Option<Self> {
        Some(Self::Offline)
    }

    fn is_top(&self) -> bool {
        false
    }
}

impl Join for FsPolicy {
    fn join(self, other: Self) -> Self {
        Self {
            read_prefixes: union_prefixes(self.read_prefixes, other.read_prefixes),
            write_prefixes: union_prefixes(self.write_prefixes, other.write_prefixes),
            deny_paths: union_prefixes(self.deny_paths, other.deny_paths),
        }
    }
}

impl Axis for FsPolicy {
    fn join_top(self) -> Option<Self> {
        (!self.deny_paths.is_empty()).then(|| Self {
            read_prefixes: vec![FrozenPath::root()],
            write_prefixes: vec![FrozenPath::root()],
            deny_paths: self.deny_paths,
        })
    }

    /// Writing reads, so writing the root reads it too; the root as its frozen
    /// identity holds it, so a symlink to `/` does.
    fn is_top(&self) -> bool {
        self.deny_paths.is_empty() && self.write_prefixes.iter().any(FrozenPath::is_universal)
    }
}

impl<V: Ord + VariantArray> Join for BTreeSet<V> {
    fn join(mut self, other: Self) -> Self {
        self.extend(other);
        self
    }
}

impl<V: Ord + VariantArray> Axis for BTreeSet<V> {
    fn join_top(self) -> Option<Self> {
        None
    }

    fn is_top(&self) -> bool {
        self.len() == V::VARIANTS.len()
    }
}

fn union_prefixes(a: Vec<FrozenPath>, b: Vec<FrozenPath>) -> Vec<FrozenPath> {
    a.into_iter()
        .chain(b)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
