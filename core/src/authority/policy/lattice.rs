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
//! [`Capabilities`] is the authored value: what a grant file decodes to, and
//! what [`GrantLayer::from_capabilities`] resolves for a layer written in code.
//! Nothing judges it and no layer keeps it.
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
use crate::syntax::quote::quote_word;
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
/// [`GrantLayer::extend`] consumes one, and only the loader makes one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment(GrantLayer);

impl Fragment {
    /// Only `load::profile` calls this.
    pub(crate) fn of(layer: GrantLayer) -> Self {
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

/// An exec key as the author wrote it: the written twin of
/// `authority::policy::exec::{ExecAllow, ExecDeny}`, which resolve it.
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

/// Exec authority as authored. A key in both sets is Both.
///
/// The author's spellings, which [`GrantLayer::from_capabilities`] resolves
/// into the rules of `authority::policy::exec`; nothing judges a grant of this form.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecGrant {
    every: bool,
    #[serde(default)]
    allow: BTreeSet<ExecKey>,
    #[serde(default)]
    deny: BTreeSet<ExecKey>,
}

impl ExecGrant {
    pub fn of(
        allow: impl IntoIterator<Item = ExecKey>,
        deny: impl IntoIterator<Item = ExecKey>,
    ) -> Self {
        Self {
            every: false,
            allow: allow.into_iter().collect(),
            deny: deny.into_iter().collect(),
        }
    }

    /// This grant, admitting every program less its exclusions.
    #[must_use]
    pub fn with_every(self) -> Self {
        Self {
            every: true,
            ..self
        }
    }

    pub fn every(&self) -> bool {
        self.every
    }

    pub fn allows(&self) -> impl Iterator<Item = &ExecKey> {
        self.allow.iter()
    }

    pub fn denies(&self) -> impl Iterator<Item = &ExecKey> {
        self.deny.iter()
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

/// The per-effect policies as authored: every place a spelling, nothing yet
/// resolved.
///
/// [`GrantLayer::from_capabilities`] resolves this authored form into a layer.
/// Runtime records instead cross `shell::grant::decode_capability_map`, which
/// walks each spelling as it freezes it. Either way the layer keeps the
/// resolution, not this authored form.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    #[serde(default)]
    pub exec: Option<ExecGrant>,
    #[serde(default)]
    pub fs: Option<FsPolicy>,
    #[serde(default)]
    pub net: Option<Net>,
    #[serde(default)]
    pub gpu: Option<Gpu>,
    /// `None` inherits, so silence permits as on every other axis.
    #[serde(default)]
    pub editor: Option<BTreeSet<EditorVerb>>,
    #[serde(default)]
    pub shell: Option<BTreeSet<ShellVerb>>,
}

impl Capabilities {
    /// Lattice bottom for positive authority: every effect pinned to its
    /// most-restrictive value, so pushing it as a layer zeroes every axis.
    pub fn deny_all() -> Self {
        Self {
            exec: Some(ExecGrant::default()),
            fs: Some(FsPolicy::default()),
            net: Some(Net::Offline),
            gpu: Some(Gpu::Withheld),
            editor: Some(BTreeSet::new()),
            shell: Some(BTreeSet::new()),
        }
    }
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
/// is [`Extent`] for an allowance and [`Excluded`] for an exclusion.
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

/// The dynamic stack of layers: ambient root at index 0, innermost
/// `grant { ... }` on top.
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
    /// a received stack costs, and the only thing a receiver does to it.  The
    /// seed's decode and `run_framed` call it.  Never fails: an object not
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
