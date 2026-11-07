//! The capability lattice: one layer of typed authority, and the folds over it.
//!
//! A [`GrantLayer`] bundles the per-effect policies, each already resolved:
//! its spellings were walked where it was decoded, so what it holds are
//! positions and the objects its exclusions name, and nothing judges a
//! spelling again.  Composition downward is the [`GrantStack`]: a `grant { ... }`
//! or a loaded profile pushes one more layer, and every judgment folds the
//! layers afresh, `exec::rules`, `fs::region`, [`GrantStack::permits`], rather
//! than flattening them into one layer first.  [`GrantLayer::extend`] is the
//! one place a layer still composes eagerly: `--extend` joins a [`Fragment`]
//! onto a base ceiling at load time, before any attenuation runs.
//!
//! A grant is authored as a record, its *grant value*, and resolved by one
//! front-end, `shell::grant::decode_capability_map`, on the host that
//! enforces it; a layer keeps the resolution, never the record.
//!
//! Every policy denotes a function from requests to Belnap's four values,
//! [`Evidence`].  Restriction is truth meet ([`Meet`]), extension information
//! join ([`Join`]); an omitted axis is `t` in a policy and `⊥` in a fragment,
//! and the two roles are two types.  Only the enforcing boundary closes
//! evidence to a decision ([`Evidence::permits`]).
//!
//! This is data, with no reader of its own: the in-process guard
//! (`crate::authority::guard`) and the OS sandbox (`crate::authority::sandbox`) are its two
//! consumers.

use super::exec::ExecRules;
use super::fs::FsLayer;
use crate::path::{FrozenPath, RealPath, Subject, Trace};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
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

/// A resolved axis policy, as a fragment over the unrestricted policy.
pub(crate) trait Axis: Join + Sized {
    /// `t ⊕ self`; `None`, which is `t`, where omission loses nothing.
    fn join_top(self) -> Option<Self>;
    /// Denotes `t`: restricts nothing.
    fn is_top(&self) -> bool;
}

/// An extension fragment: an omitted axis states nothing (`⊥`), where an
/// omitted axis of a [`GrantLayer`] restricts nothing (`t`).  Only
/// [`GrantLayer::extend`] consumes one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment(GrantLayer);

impl Fragment {
    /// `layer` read as an extension: an axis it omits now states nothing.
    pub fn of(layer: GrantLayer) -> Self {
        Self(layer)
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

/// The GPU a layer withholds; the veto is sticky.
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
pub enum Gpu {
    Withheld,
}

/// A verb a layer may withhold, gating a builtin rather than an access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    Editor(EditorVerb),
    Shell(ShellVerb),
}

impl Flag {
    /// Whether a layer lists the verb; `None` abstains.
    fn vote(self, layer: &GrantLayer) -> Option<bool> {
        match self {
            Self::Editor(v) => layer.editor.as_ref().map(|s| s.contains(&v)),
            Self::Shell(v) => layer.shell.as_ref().map(|s| s.contains(&v)),
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

/// Where an allowance holds: everywhere (the root spelling `/`, on every
/// host, or a spelling whose walk reached the root on a host whose root is
/// `/`), or under a position.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Extent {
    Everywhere,
    Under {
        position: RealPath,
        /// What the walk to `position` passed ([`Trace::passed`]).
        passed: Vec<RealPath>,
    },
}

impl Extent {
    /// Whether this extent contains `subject`: everywhere contains every
    /// subject, a position what lies beneath it and never everywhere.
    pub(crate) fn contains(&self, subject: Subject<'_>) -> bool {
        match self {
            Self::Everywhere => true,
            Self::Under { position, .. } => subject.beneath(position),
        }
    }

    /// The position this extent holds from; `None` for everywhere, which no
    /// walk makes on Windows.
    pub(crate) fn real(&self) -> Option<&RealPath> {
        match self {
            Self::Everywhere => None,
            Self::Under { position, .. } => Some(position),
        }
    }

    /// The position this extent holds from, `/` for everywhere: a position
    /// where `/` is a root.
    #[cfg(unix)]
    pub(crate) fn position(&self) -> std::borrow::Cow<'_, RealPath> {
        self.real().map_or_else(
            || std::borrow::Cow::Owned(RealPath::universal()),
            std::borrow::Cow::Borrowed,
        )
    }
}

/// A grant spelling in force: as written, and what the walk made of it.  `R`
/// is [`Extent`] for an allowance and [`Excluded`](crate::path::Excluded)
/// for an exclusion.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GrantPath<R> {
    pub written: FrozenPath,
    pub real: R,
}

impl GrantPath<Extent> {
    /// What holds everywhere, as the root spelling says it.
    pub(crate) fn everywhere() -> Self {
        Self {
            written: FrozenPath::root(),
            real: Extent::Everywhere,
        }
    }
}

impl GrantPath<Trace> {
    /// This walk read as an allowance's extent: everywhere if it reached the
    /// root, as the root spelling does, else the position it reached and what
    /// it passed on the way.
    pub(crate) fn extent(self) -> GrantPath<Extent> {
        let trace = &self.real;
        GrantPath {
            real: if trace.real().is_universal() {
                Extent::Everywhere
            } else {
                Extent::Under {
                    position: trace.real().clone(),
                    passed: trace.passed().to_vec(),
                }
            },
            written: self.written,
        }
    }
}

/// A layer in force: its rules, resolved, and the axes that name no path.
///
/// One representation: there is no authored copy beside it, and whatever shows
/// a layer (exarch's prompt, a refusal) reads `written` off its rules.  `None`
/// on an axis is no opinion.  Serialises as its traces and deserialises as
/// they were, for [`GrantLayer::rehold`] to take hold of again on the receiving
/// host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantLayer {
    fs: Option<FsLayer>,
    exec: Option<ExecRules>,
    net: Option<Net>,
    gpu: Option<Gpu>,
    editor: Option<BTreeSet<EditorVerb>>,
    shell: Option<BTreeSet<ShellVerb>>,
}

impl GrantLayer {
    /// A layer of these axes, each already resolved: what the decoder makes
    /// of a record.
    pub(crate) fn new(
        fs: Option<FsLayer>,
        exec: Option<ExecRules>,
        net: Option<Net>,
        gpu: Option<Gpu>,
        editor: Option<BTreeSet<EditorVerb>>,
        shell: Option<BTreeSet<ShellVerb>>,
    ) -> Self {
        Self {
            fs,
            exec,
            net,
            gpu,
            editor,
            shell,
        }
    }

    /// Ambient authority, the lattice top: all axes `None`, no attenuation.
    pub fn root() -> Self {
        Self::default()
    }

    pub(crate) fn fs(&self) -> Option<&FsLayer> {
        self.fs.as_ref()
    }

    pub(crate) fn exec(&self) -> Option<&ExecRules> {
        self.exec.as_ref()
    }

    /// `self ◁ fragment`: the ceiling a base and an extension make, before any
    /// restriction.  An exclusion of the fragment always wins; an allowance
    /// never removes authority; an axis the base omits stays unrestricted,
    /// less the fragment's exclusions.
    #[must_use]
    pub fn extend(self, fragment: Fragment) -> Self {
        let Fragment(fragment) = fragment;
        Self {
            fs: extend(self.fs, fragment.fs),
            exec: extend(self.exec, fragment.exec),
            net: extend(self.net, fragment.net),
            gpu: extend(self.gpu, fragment.gpu),
            editor: extend(self.editor, fragment.editor),
            shell: extend(self.shell, fragment.shell),
        }
    }

    /// True iff some supplied axis does not denote `t`.
    pub fn is_restrictive(&self) -> bool {
        fn restricts<T: Axis>(axis: Option<&T>) -> bool {
            axis.is_some_and(|a| !a.is_top())
        }
        restricts(self.fs.as_ref())
            || restricts(self.exec.as_ref())
            || restricts(self.net.as_ref())
            || restricts(self.gpu.as_ref())
            || restricts(self.editor.as_ref())
            || restricts(self.shell.as_ref())
    }

    /// Take hold again of every exclusion's objects on this host.
    pub(crate) fn rehold(&mut self) {
        if let Some(fs) = &mut self.fs {
            fs.rehold();
        }
        if let Some(exec) = &mut self.exec {
            exec.rehold();
        }
    }

    /// This layer, its exec beside every program: the table an extension of
    /// an omitted exec axis makes, which no grant spells.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_every(self) -> Self {
        let every = ExecRules::new([super::exec::ExecAllow::Any], []);
        Self {
            exec: Some(self.exec.unwrap_or_default().join(every)),
            ..self
        }
    }

    /// What this layer shows of itself, `None` where it has nothing to say: it
    /// restricts no axis and its exec runs nothing by name.  Under a nested fs
    /// restriction a run place stays readable, which the reader needs to know
    /// even of a layer that denotes `t`.
    pub fn summary(&self) -> Option<LayerSummary> {
        let exec = self.exec.as_ref().map(ExecRules::summary);
        let runs = exec
            .as_ref()
            .is_some_and(|e| e.every || !e.allows.is_empty());
        (self.is_restrictive() || runs).then(|| LayerSummary {
            fs: self.fs.as_ref().map(FsLayer::summary),
            exec,
            net: self.net,
            gpu: self.gpu,
            editor: self.editor.clone(),
            shell: self.shell.clone(),
        })
    }
}

/// A stack as it is shown: each layer's spellings and axes, nothing a host
/// could judge with, and whether the host that showed it can withhold the GPU.
///
/// What `Probe::Grants` answers and what a prompt reads, from a local engine
/// and a remote one alike.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantSummary {
    layers: Vec<LayerSummary>,
    withholds_gpu: bool,
}

impl GrantSummary {
    pub fn layers(&self) -> &[LayerSummary] {
        &self.layers
    }

    /// Whether a `` gpu: `withheld `` among the layers is held by the host
    /// that showed them; `false` on Windows, where it changes nothing.
    pub fn withholds_gpu(&self) -> bool {
        self.withholds_gpu
    }

    /// A summary crossed from elsewhere, for tests.
    #[cfg(any(test, feature = "test-util"))]
    pub fn of(layers: Vec<LayerSummary>, withholds_gpu: bool) -> Self {
        Self {
            layers,
            withholds_gpu,
        }
    }
}

/// No layers, on this host.
impl Default for GrantSummary {
    fn default() -> Self {
        GrantStack::default().summary()
    }
}

/// A layer as it is shown: its spellings and axes, nothing a host could judge
/// with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerSummary {
    /// `read`, `write`, `deny`: each rule's `written`, shown.
    pub fs: Option<FsSummary>,
    /// `every`, and `allows`/`denies` as the grant spells each key —
    /// `` `name git ``, `` `absolute /usr/bin/git ``, `` `dir `absolute /opt ``.
    pub exec: Option<ExecSummary>,
    pub net: Option<Net>,
    pub gpu: Option<Gpu>,
    pub editor: Option<BTreeSet<EditorVerb>>,
    pub shell: Option<BTreeSet<ShellVerb>>,
}

/// What a layer reads, writes and excludes, as written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsSummary {
    pub read: Vec<String>,
    pub write: Vec<String>,
    pub deny: Vec<String>,
}

/// What a layer runs and excludes, as written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecSummary {
    pub every: bool,
    pub allows: Vec<String>,
    pub denies: Vec<String>,
}

/// A shell's stack holds the ambient root at index 0, innermost `grant { ... }`
/// on top; a stack composed for installation, such as a root's grants, holds
/// only its own layers.
///
/// A newtype so the folds over it live together rather than being respelled
/// as `iter().any(...)` at each call site; `transparent` serde keeps it free
/// at every boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GrantStack(Vec<GrantLayer>);

impl GrantStack {
    /// Where every shell starts; `grant { ... }` blocks and the session-wide
    /// `--capabilities` ceiling push attenuating layers on top.
    pub fn root() -> Self {
        Self(vec![GrantLayer::root()])
    }

    /// True iff some layer restricts (denotes other than `t`) — what
    /// `Shell::has_active_capabilities` reports.
    pub fn is_restrictive(&self) -> bool {
        self.0.iter().any(GrantLayer::is_restrictive)
    }

    /// Whether every layer is the ambient root: a stack that holds no position
    /// and no identity, so means the same on every host.
    pub fn is_root(&self) -> bool {
        self.0.iter().all(|l| *l == GrantLayer::root())
    }

    /// Whether no layer restricts exec: the one question of exec denoting `t`.
    pub fn exec_is_top(&self) -> bool {
        self.exec().all(Axis::is_top)
    }

    /// Whether no layer restricts fs: the one question of fs denoting `t`.
    pub fn fs_is_top(&self) -> bool {
        self.fs().all(Axis::is_top)
    }

    pub fn push(&mut self, layer: GrantLayer) {
        self.0.push(layer);
    }

    /// Remove the `n` layers pushed at `at`, keeping any session layer pushed
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

    pub fn layers(&self) -> impl Iterator<Item = &GrantLayer> {
        self.0.iter()
    }

    /// What the stack shows of itself: the layers that have something to say.
    pub fn summary(&self) -> GrantSummary {
        GrantSummary {
            layers: self.0.iter().filter_map(GrantLayer::summary).collect(),
            withholds_gpu: crate::authority::sandbox::WITHHOLDS_GPU,
        }
    }

    /// Take hold again of every exclusion's object on this host: the one walk
    /// a received stack costs, and the only thing a receiver does to it.  A
    /// seed's decode and a root's boot call it.  Never fails: an object not
    /// found at its position stays unheld.
    pub fn rehold(&mut self) {
        self.0.iter_mut().for_each(GrantLayer::rehold);
    }

    /// The identity of every object the fs exclusions hold, `None` where one
    /// is unheld: what a test asks of a receiver's `rehold`.
    #[cfg(all(test, unix))]
    pub(crate) fn held_ids(&self) -> Vec<Option<crate::path::ObjectId>> {
        let denies = self
            .fs()
            .flat_map(|fs| fs.table(&super::FsOp::Read).denies());
        denies
            .flat_map(|deny| deny.grant().real.held_ids())
            .collect()
    }

    /// Layers with no opinion are skipped, here and in `fs`, `net` and `gpu` below:
    /// the folds intersect what remains, and an empty run means no attenuation.
    pub(crate) fn exec(&self) -> impl Iterator<Item = &ExecRules> {
        self.0.iter().filter_map(|l| l.exec.as_ref())
    }

    pub(crate) fn fs(&self) -> impl Iterator<Item = &FsLayer> {
        self.0.iter().filter_map(|l| l.fs.as_ref())
    }

    pub fn net(&self) -> impl Iterator<Item = Net> {
        self.0.iter().filter_map(|l| l.net)
    }

    /// Any layer's network veto binds the whole stack.
    pub fn net_veto(&self) -> Option<Net> {
        self.net().next()
    }

    pub fn gpu(&self) -> impl Iterator<Item = Gpu> {
        self.0.iter().filter_map(|l| l.gpu)
    }

    /// A meet over the layers: one `false` vote anywhere withholds `flag`
    /// whatever sits above it, and silence permits.  A verb's gate, not an OS
    /// rule, so it reaches no sandbox projection.
    pub fn permits(&self, flag: Flag) -> bool {
        self.0.iter().all(|l| flag.vote(l) != Some(false))
    }
}

impl IntoIterator for GrantStack {
    type Item = GrantLayer;
    type IntoIter = std::vec::IntoIter<GrantLayer>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
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

impl Join for Gpu {
    fn join(self, _: Self) -> Self {
        Self::Withheld
    }
}

impl Axis for Gpu {
    fn join_top(self) -> Option<Self> {
        Some(Self::Withheld)
    }

    fn is_top(&self) -> bool {
        false
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

// ── A test's grant, authored ──────────────────────────────────────────────

#[cfg(any(test, feature = "test-util"))]
pub use builder::{Capabilities, ExecGrant, ExecKey, FsPolicy};

/// A grant a test writes in Rust: it spells a grant value, which the one
/// front-end resolves as it resolves any other.
#[cfg(any(test, feature = "test-util"))]
mod builder {
    use super::{EditorVerb, Gpu, Net, ShellVerb};
    use crate::authority::grant::freeze::Place;
    use crate::data::FOValue;
    use crate::data::datum::{Datum, tag};
    use crate::path::FrozenPath;
    use std::collections::{BTreeMap, BTreeSet};

    /// An exec key: a bare name, a file, or every program beneath a directory.
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    pub enum ExecKey {
        Name(String),
        Path(FrozenPath),
        Dir(FrozenPath),
    }

    /// Exec authority as written. A key in both sets is Both.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct ExecGrant {
        allow: BTreeSet<ExecKey>,
        deny: BTreeSet<ExecKey>,
    }

    impl ExecGrant {
        pub fn of(
            allow: impl IntoIterator<Item = ExecKey>,
            deny: impl IntoIterator<Item = ExecKey>,
        ) -> Self {
            Self {
                allow: allow.into_iter().collect(),
                deny: deny.into_iter().collect(),
            }
        }

        pub fn allows(&self) -> impl Iterator<Item = &ExecKey> {
            self.allow.iter()
        }

        pub fn denies(&self) -> impl Iterator<Item = &ExecKey> {
            self.deny.iter()
        }
    }

    /// Filesystem access as written: `deny_paths` carve out subtrees no
    /// access may touch, even under a covering prefix.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct FsPolicy {
        pub read_prefixes: Vec<FrozenPath>,
        pub write_prefixes: Vec<FrozenPath>,
        pub deny_paths: Vec<FrozenPath>,
    }

    /// The per-effect policies as written, `None` inheriting: every path an
    /// `absolute` spelling, nothing resolved.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct Capabilities {
        pub exec: Option<ExecGrant>,
        pub fs: Option<FsPolicy>,
        pub net: Option<Net>,
        pub gpu: Option<Gpu>,
        pub editor: Option<BTreeSet<EditorVerb>>,
        pub shell: Option<BTreeSet<ShellVerb>>,
    }

    impl Capabilities {
        /// The grant value this spells, as an author writes it in ral.
        pub fn value(self) -> FOValue {
            let Self {
                exec,
                fs,
                net,
                gpu,
                editor,
                shell,
            } = self;
            let entries = [
                exec.map(|e| {
                    let targets = |keys: &BTreeSet<ExecKey>| keys.iter().map(target).collect();
                    (
                        "exec",
                        table([("allow", targets(&e.allow)), ("deny", targets(&e.deny))]),
                    )
                }),
                fs.map(|f| {
                    let places = |paths: &[FrozenPath]| paths.iter().map(place).collect();
                    let read = ("read", places(&f.read_prefixes));
                    let write = ("write", places(&f.write_prefixes));
                    ("fs", table([read, write, ("deny", places(&f.deny_paths))]))
                }),
                net.map(|n| ("net", tag(n.into(), None))),
                gpu.map(|g| ("gpu", tag(g.into(), None))),
                editor.map(|v| ("editor", verbs(v))),
                shell.map(|v| ("shell", verbs(v))),
            ];
            let entries = entries.into_iter().flatten();
            (entries.map(|(key, value)| (key.to_owned(), value)))
                .collect::<BTreeMap<_, _>>()
                .encode()
        }
    }

    fn place(path: &FrozenPath) -> FOValue {
        Place::Absolute(path.as_str().to_owned()).value()
    }

    fn target(key: &ExecKey) -> FOValue {
        match key {
            ExecKey::Name(name) => tag("name", Some(name.clone().encode())),
            ExecKey::Path(file) => place(file),
            ExecKey::Dir(dir) => tag("dir", Some(place(dir))),
        }
    }

    fn table<const N: usize>(lists: [(&str, Vec<FOValue>); N]) -> FOValue {
        (lists.into_iter().map(|(key, list)| (key.to_owned(), list)))
            .collect::<BTreeMap<_, _>>()
            .encode()
    }

    fn verbs<V: Into<&'static str>>(set: BTreeSet<V>) -> FOValue {
        (set.into_iter().map(|v| tag(v.into(), None)))
            .collect::<Vec<_>>()
            .encode()
    }
}
