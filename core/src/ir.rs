//! Call-by-push-value intermediate representation: the target of
//! elaboration ([`crate::elaborator`]), the input to evaluation.
//!
//! [`Val`] is inert data and [`Comp`] is effectful, so a value can never
//! diverge or perform I/O.  The [`Spanned`] wrapper puts a source range on
//! every [`Comp`] and on each sub-`Val` position the typechecker narrows
//! onto, so a span rides with the value rather than the parent; `None`
//! means the node is synthetic — a builtin, the prelude, generated code.

mod op;
mod pattern;
mod redirect;

pub use op::{ArithOp, BinaryOp, CompareOp, EqOp};
pub use pattern::{FieldPattern, Pattern};
pub use redirect::{Redirect, Redirects, StderrTarget, StdinSource, WriteMode};

use crate::data::Finite;
use crate::data::Str;
use crate::path::TildePath;
use crate::source::{Span, Spanned};
use crate::ty::Site;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::fmt::Debug;
use std::sync::Arc;

// ── Phases ──────────────────────────────────────────────────────────────

/// What each annotated position of a tree holds: [`Checked`] in a tree the
/// evaluator runs, the checker's own phase while it infers one.
pub trait Phase: Debug + Clone + PartialEq {
    /// How a [`CompKind::Bind`]'s or a `Define`'s right-hand side is bound.
    type Bind: Debug + Clone + PartialEq;
    /// Where a [`CompKind::App`] forces a returned block between arguments.
    type Call: Debug + Clone + PartialEq;
    /// A [`CompKind::BindingCall`]'s; uninhabited once checked.
    type BindingCall: Debug + Clone + PartialEq;
    /// The type an [`Exec`]'s door admits a value against.
    type Boundary: Debug + Clone + PartialEq;
    /// A [`Val::Protected`]'s contract.
    type Membrane: Debug + Clone + PartialEq;
}

/// A phase whose trees serialise: bind and call notes are skipped and read
/// back as their defaults, while boundary and membrane sites are written out.
pub trait Serial:
    Phase<
        Bind: Default,
        Call: Default,
        Boundary: Serialize + DeserializeOwned,
        Membrane: Serialize + DeserializeOwned,
    >
{
}

impl<P> Serial for P where
    P: Phase<
            Bind: Default,
            Call: Default,
            Boundary: Serialize + DeserializeOwned,
            Membrane: Serialize + DeserializeOwned,
        >
{
}

/// No holes: every elaboration is explicit in the tree.  The elaborator's
/// output shares this phase with the checker's, which alone carries sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checked {}

impl Phase for Checked {
    type Bind = ();
    type Call = ();
    type BindingCall = Infallible;
    type Boundary = Arc<Site>;
    type Membrane = Arc<Site>;
}

// ── Values ──────────────────────────────────────────────────────────────

/// An identifier the IR binds or mentions: a variable, a pattern name, a
/// bare command head, a variant label. Shared, not copied — a bind, a
/// capture and a label clone a pointer.
pub type Name = Arc<str>;

/// A binder only the compiler writes: no source text can name it.
pub(crate) fn synthetic(tag: &str, n: usize) -> Name {
    format!("%{tag}{n}").into()
}

pub(crate) fn is_synthetic(name: &str) -> bool {
    name.starts_with('%')
}

/// Every name a shape mentions, bound or free: sorted, distinct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occ(Arc<[Name]>);

impl Occ {
    pub(crate) fn of(shape: &impl Mentions) -> Self {
        let mut refs = Vec::new();
        shape.mentions(&mut refs);
        refs.sort_unstable();
        refs.dedup();
        Self(refs.into_iter().cloned().collect())
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.0.binary_search_by(|n| n.as_ref().cmp(name)).is_ok()
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &Name> {
        self.0.iter()
    }
}

/// What a node's shape mentions, bound or free — no wildcard arm anywhere in
/// any `impl`, so a new variant is a compile error here rather than a
/// silently missed name. Over-approximate: every name, bound or free, taken
/// branch or not.
///
/// A nested [`Node`] contributes its own stored [`Occ`] rather than being
/// walked again, so computing occ for a whole program is linear in its size.
pub(crate) trait Mentions {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>);
}

/// IR that closes, with what it mentions computed once, where it is built.
#[derive(Debug, Clone)]
pub struct Node<S> {
    shape: S,
    occ: Occ,
}

impl<S> Node<S> {
    pub(crate) fn new(shape: S) -> Arc<Self>
    where
        S: Mentions,
    {
        Arc::new(Self::build(shape))
    }

    fn build(shape: S) -> Self
    where
        S: Mentions,
    {
        let occ = Occ::of(&shape);
        Self { shape, occ }
    }

    pub fn shape(&self) -> &S {
        &self.shape
    }

    pub(crate) fn occ(&self) -> &Occ {
        &self.occ
    }
}

impl<S: PartialEq> PartialEq for Node<S> {
    fn eq(&self, other: &Self) -> bool {
        self.shape == other.shape
    }
}

/// The wire carries the shape alone; decoding recomputes occ, so no occ is
/// ever read off it.
impl<S: Mentions> From<S> for Node<S> {
    fn from(shape: S) -> Self {
        Self::build(shape)
    }
}

impl<S: Serialize> Serialize for Node<S> {
    fn serialize<Ser: serde::Serializer>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error> {
        self.shape.serialize(serializer)
    }
}

impl<'de, S: Mentions + Deserialize<'de>> Deserialize<'de> for Node<S> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        S::deserialize(deserializer).map(Self::from)
    }
}

pub type ThunkNode<P = Checked> = Node<Arc<Comp<P>>>;
pub type GroupNode<P = Checked> = Node<Box<[(Name, Arc<Comp<P>>)]>>;
pub type ListNode<P = Checked> = Node<Box<[Spanned<Val<P>>]>>;
/// Entries sorted by key, stably; equal keys are the checker's to refuse.
pub type FieldsNode<P = Checked> = Node<Box<[(Name, Spanned<Val<P>>)]>>;

impl<P: Phase> Mentions for Arc<Comp<P>> {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        self.as_ref().mentions(out);
    }
}

impl<P: Phase> Mentions for Box<[(Name, Arc<Comp<P>>)]> {
    // Every member's: the group is one unit.
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        for (_name, member) in self {
            member.mentions(out);
        }
    }
}

impl<P: Phase> Mentions for Box<[Spanned<Val<P>>]> {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        for elem in self {
            elem.item.mentions(out);
        }
    }
}

impl<P: Phase> Mentions for Box<[(Name, Spanned<Val<P>>)]> {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        for (_key, value) in self {
            value.item.mentions(out);
        }
    }
}

/// The head word of a command, in the shape the source wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum CommandName {
    Bare(Name),
    /// A literal path, bearing a separator under the host's rules
    /// ([`crate::path::names_a_path`]): skips the lookup chain, exec'd as
    /// written.
    Path(String),
    /// `~/x`, carried unexpanded until command resolution.
    TildePath(TildePath),
}

impl CommandName {
    /// A head named by a string at run time, classified as the lexer
    /// classifies a source word: a path when it bears a separator under the
    /// host's rules, else a bare name.  A `~` here is text, not home: home is
    /// source syntax, which a string has left behind.
    pub(crate) fn of(name: &str) -> Self {
        if crate::path::names_a_path(name) {
            Self::Path(name.into())
        } else {
            Self::Bare(name.into())
        }
    }

    /// The head as the source wrote it — what a diagnostic raised before the
    /// run can name it.  A `~` stays a `~`: expanding it wants a live `HOME`,
    /// which is command resolution's business rather than the checker's.
    pub(crate) fn written(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Bare(name) => std::borrow::Cow::Borrowed(name),
            Self::Path(name) => std::borrow::Cow::Borrowed(name),
            Self::TildePath(path) => std::borrow::Cow::Owned(path.to_literal()),
        }
    }
}

/// The CBPV value category: inert data, requiring no evaluation.  The typed
/// literals exist so that `$[…]` lowers into plain `Bind` sequences rather
/// than round-tripping through string parsing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound = "P: Serial")]
pub enum Val<P: Phase = Checked> {
    Unit,
    String(Str),
    Int(i64),
    Float(Finite),
    Bool(bool),
    Variable(Name),
    /// A suspended computation, eliminated by [`CompKind::Force`]: `⟨M,
    /// ρ|occ(M)⟩`.
    Thunk(Arc<ThunkNode<P>>),
    /// A plain list literal: no spread, so every element is here already.
    List(Arc<ListNode<P>>),
    /// `[key: val, …]`: static labels, no spread.
    Record(Arc<FieldsNode<P>>),
    /// `[:, key: val, …]`: keys written out, no spread.
    Map(Arc<FieldsNode<P>>),
    /// The label has no leading backtick; a bare tag carries `Unit`.
    Variant {
        label: Name,
        payload: Box<Self>,
    },
    /// Checker-inserted protection at an instantiation site.
    Protected {
        value: Box<Self>,
        site: P::Membrane,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound = "P: Serial")]
pub enum Key<P: Phase = Checked> {
    Label(Str),
    Position(usize),
    Data(Val<P>),
}

impl<P: Phase> Val<P> {
    /// `thunk M`, from an already-built `M`.
    pub(crate) fn thunk(comp: Arc<Comp<P>>) -> Self {
        Self::Thunk(Node::new(comp))
    }

    /// A plain list literal with no spread.
    pub(crate) fn list(items: impl Into<Box<[Spanned<Self>]>>) -> Self {
        Self::List(Node::new(items.into()))
    }

    /// `[key: val, …]`, entries already sorted by key.
    pub(crate) fn record(entries: impl Into<Box<[(Name, Spanned<Self>)]>>) -> Self {
        Self::Record(Node::new(entries.into()))
    }

    /// `[:, key: val, …]`, entries already sorted by key.
    pub(crate) fn map(entries: impl Into<Box<[(Name, Spanned<Self>)]>>) -> Self {
        Self::Map(Node::new(entries.into()))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound = "P: Serial")]
pub enum ValListElem<P: Phase = Checked> {
    Single(Spanned<Val<P>>),
    /// `...x`, spliced into the surrounding list, its `...` included in an argument span.
    Spread(Spanned<Val<P>>),
}

impl<P: Phase> ValListElem<P> {
    /// The slot's value, whichever form the slot takes.
    pub(crate) fn slot(&self) -> &Spanned<Val<P>> {
        match self {
            Self::Single(v) | Self::Spread(v) => v,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound = "P: Serial")]
pub enum MapEntryV<P: Phase = Checked> {
    Entry(Spanned<Val<P>>, Spanned<Val<P>>),
    Spread(Spanned<Val<P>>),
}

/// A command's argv — the same slots a list literal has, a `...` splicing a
/// list in place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound = "P: Serial")]
#[serde(transparent)]
pub struct Argv<P: Phase = Checked>(Vec<ValListElem<P>>);

impl<P: Phase> Default for Argv<P> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<P: Phase> Argv<P> {
    /// The slots as a literal positional list, or `None` if any is a
    /// `Spread` — dynamic arity, so callers fall back to weaker checks.
    pub(crate) fn positional(&self) -> Option<Vec<&Spanned<Val<P>>>> {
        self.0
            .iter()
            .map(|e| match e {
                ValListElem::Single(v) => Some(v),
                ValListElem::Spread(_) => None,
            })
            .collect()
    }

    /// How many arguments there are, unless a spread makes that dynamic.
    pub(crate) fn arity(&self) -> Option<usize> {
        self.positional().map(|p| p.len())
    }
}

impl<P: Phase> std::ops::Deref for Argv<P> {
    type Target = [ValListElem<P>];
    fn deref(&self) -> &[ValListElem<P>] {
        &self.0
    }
}

impl<P: Phase> From<Vec<ValListElem<P>>> for Argv<P> {
    fn from(elems: Vec<ValListElem<P>>) -> Self {
        Self(elems)
    }
}

impl<P: Phase> FromIterator<ValListElem<P>> for Argv<P> {
    fn from_iter<I: IntoIterator<Item = ValListElem<P>>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl<P: Phase> Extend<ValListElem<P>> for Argv<P> {
    fn extend<I: IntoIterator<Item = ValListElem<P>>>(&mut self, iter: I) {
        self.0.extend(iter);
    }
}

impl<P: Phase> IntoIterator for Argv<P> {
    type Item = ValListElem<P>;
    type IntoIter = std::vec::IntoIter<ValListElem<P>>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a, P: Phase> IntoIterator for &'a Argv<P> {
    type Item = &'a ValListElem<P>;
    type IntoIter = std::slice::Iter<'a, ValListElem<P>>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

// ── Phrases ─────────────────────────────────────────────────────────────

/// A whole program (or `source`d file): the phrases run in order, each
/// extending the environment the next one sees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Toplevel {
    pub phrases: Vec<Spanned<Phrase>>,
    /// Stored session data the checker solved this unit's uses of, each to be
    /// admitted against that type before the first phrase runs.
    pub admits: Vec<(String, Arc<crate::ty::Site>)>,
}

/// A `Phrase::Define`'s names, each with its checker-closed scheme.
pub type DefineSchemes = Vec<(String, crate::ty::Closed)>;

/// One top-level statement, checked unless `D` says it is not: the schemes
/// are the checker's, so elaboration holds none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(
    serialize = "D: Serialize, P: Serial",
    deserialize = "D: Deserialize<'de>, P: Serial"
))]
pub enum Phrase<D = DefineSchemes, P: Phase = Checked> {
    /// `let p = M` at the top level: run M, extend the session environment.
    /// `schemes` has one entry per name `pattern` binds, closed and
    /// generalised by the checker — a destructuring `let` carries one per
    /// component.
    Define {
        pattern: Arc<Pattern>,
        comp: Arc<Comp<P>>,
        #[serde(skip)]
        hole: P::Bind,
        schemes: D,
    },
    /// Any other statement.
    Run(Arc<Comp<P>>),
}

/// What elaboration yields, and the checker alone turns into a [`Toplevel`].
pub type Unchecked = Vec<Spanned<Phrase<()>>>;

impl<P: Phase<Bind = ()>> CompKind<P> {
    /// `comp` to `pattern`, then `rest`.
    pub(crate) fn bind(
        pattern: Pattern,
        comp: impl Into<Arc<Comp<P>>>,
        rest: impl Into<Arc<Comp<P>>>,
    ) -> Self {
        Self::Bind {
            comp: comp.into(),
            hole: (),
            pattern: Arc::new(pattern),
            rest: rest.into(),
        }
    }
}

impl Toplevel {
    /// True if this is a single external/builtin command call.
    ///
    /// A fact about the input's shape, which hosts read to tailor what they say
    /// about a failure: exactly one phrase, `Run(c)`, with `c` an `Exec`.  A
    /// tail `Run` is never captured, so nothing wraps it.
    pub(crate) fn is_single_command(&self) -> bool {
        let [phrase] = self.phrases.as_slice() else {
            return false;
        };
        let Phrase::Run(comp) = &phrase.item else {
            return false;
        };
        matches!(comp.item, CompKind::Exec(_))
    }

    /// The scheme each top-level name was closed at, the last `let` of a name
    /// winning as it does in the environment the phrases build.
    pub(crate) fn exported_schemes(&self) -> Vec<(String, crate::ty::Closed)> {
        let last: std::collections::BTreeMap<_, _> = self
            .phrases
            .iter()
            .filter_map(|phrase| match &phrase.item {
                Phrase::Define { schemes, .. } => Some(schemes),
                Phrase::Run(_) => None,
            })
            .flatten()
            .map(|(name, scheme)| (name.clone(), scheme.clone()))
            .collect();
        last.into_iter().collect()
    }

    /// Every name any phrase can reference — the phrase-level analogue of
    /// [`Mentions`], for the same lease ledger.
    pub(crate) fn referenced_names(&self) -> Vec<&str> {
        let mut out = Vec::new();
        for phrase in &self.phrases {
            match &phrase.item {
                Phrase::Define { comp, .. } | Phrase::Run(comp) => comp.mentions(&mut out),
            }
        }
        out.into_iter().map(AsRef::as_ref).collect()
    }
}

// ── Computations ────────────────────────────────────────────────────────

/// A computation with the source span elaboration gave it — the type the
/// evaluator interprets.
pub type Comp<P = Checked> = Spanned<CompKind<P>>;

impl<P: Phase> Comp<P> {
    /// The λ this code opens with, through a `Rec` projection.  Syntax, not
    /// type: a block of function type need not open with one, so only a
    /// description of a value reads it.
    pub(crate) fn lam(&self) -> Option<(&Name, &Arc<Self>)> {
        match &self.item {
            CompKind::Lam { param, body } => Some((param, body)),
            CompKind::Rec { group, index } => group.shape()[*index].1.lam(),
            _ => None,
        }
    }
}

impl<P: Phase> Mentions for Comp<P> {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        stacker::maybe_grow(64 * 1024, 2 * 1024 * 1024, || match &self.item {
            CompKind::Lam { param: _, body }
            | CompKind::Capture(body)
            | CompKind::Protect { body, .. } => body.mentions(out),
            CompKind::Bind {
                comp,
                hole: _,
                pattern: _,
                rest,
            }
            | CompKind::Seq { first: comp, rest } => {
                comp.mentions(out);
                rest.mentions(out);
            }
            CompKind::App {
                head,
                args,
                hole: _,
            } => {
                head.mentions(out);
                args.mentions(out);
            }
            CompKind::BindingCall {
                head,
                args,
                redirects,
                hole: _,
            } => {
                out.push(head);
                args.mentions(out);
                redirects.mentions(out);
            }
            CompKind::Exec(exec) => {
                exec.head.mentions(out);
                exec.argv.mentions(out);
                exec.redirects.mentions(out);
            }
            CompKind::Pipeline { stages } => {
                for stage in stages {
                    stage.mentions(out);
                }
            }
            CompKind::Prim(prim) => {
                for value in prim.operands() {
                    value.mentions(out);
                }
            }
            CompKind::Force(v) | CompKind::Return(v) => {
                v.mentions(out);
            }
            // The group is a nested node: its own occ, not a walk.
            CompKind::Rec { group, index: _ } => out.extend(group.occ().names()),
            CompKind::Tilde(_) => {}
            CompKind::If { cond, then, else_ } => {
                cond.item.mentions(out);
                then.item.mentions(out);
                else_.item.mentions(out);
            }
            CompKind::Case { scrutinee, arms } => {
                scrutinee.item.mentions(out);
                for arm in arms {
                    arm.body.item.mentions(out);
                }
            }
            CompKind::Try { body, handler } => {
                body.mentions(out);
                handler.mentions(out);
            }
            CompKind::Guard {
                acquire,
                body,
                release,
            } => {
                acquire.mentions(out);
                body.mentions(out);
                release.mentions(out);
            }
            CompKind::Within {
                opts,
                handlers,
                body,
            } => {
                out.extend(opts.occ().names());
                for arm in handlers.iter().flatten() {
                    arm.value.item.mentions(out);
                }
                body.mentions(out);
            }
            CompKind::Grant { caps, body } => {
                out.extend(caps.occ().names());
                body.mentions(out);
            }
            CompKind::Audit { body } | CompKind::Gather { body } => body.mentions(out),
            CompKind::Spawn { title, body } => {
                if let Some(t) = title {
                    t.item.mentions(out);
                }
                body.mentions(out);
            }
            CompKind::Redirect { body, redirects } => {
                body.mentions(out);
                redirects.mentions(out);
            }
        });
    }
}

impl<P: Phase> Mentions for Val<P> {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        stacker::maybe_grow(64 * 1024, 2 * 1024 * 1024, || match self {
            Self::Unit | Self::String(_) | Self::Int(_) | Self::Float(_) | Self::Bool(_) => {}
            Self::Variable(name) => out.push(name),
            Self::Protected { value, .. } => value.mentions(out),
            Self::Thunk(node) => out.extend(node.occ().names()),
            Self::List(node) => out.extend(node.occ().names()),
            Self::Record(node) | Self::Map(node) => out.extend(node.occ().names()),
            Self::Variant { payload, .. } => payload.mentions(out),
        });
    }
}

impl<P: Phase> Mentions for [ValListElem<P>] {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        for elem in self {
            elem.slot().item.mentions(out);
        }
    }
}

/// Both dispatch forms contribute their head name.  A `^name` head can
/// never reach a binding, so collecting it over-approximates — the same
/// safe direction as an untaken branch.
impl Mentions for CommandWord {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        if let CommandName::Bare(name) = self.name() {
            out.push(name);
        }
    }
}

impl<P: Phase> Mentions for Redirects<Val<P>> {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        for v in self.operands() {
            v.mentions(out);
        }
    }
}

/// The CBPV computation category — the evaluator steps a program by
/// matching on this.  Variant docs use Levy's CBPV notation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound = "P: Serial")]
pub enum CompKind<P: Phase = Checked> {
    // CBPV core.
    /// return V — produce a value.
    Return(Val<P>),
    /// force V — run a thunk.
    Force(Val<P>),
    /// λ — evaluates to a closure.
    Lam {
        param: Name,
        body: Arc<Comp<P>>,
    },
    /// `M V₁ … Vₙ`, `n ≥ 1`: the arguments are pushed, then `head` runs —
    /// taken when the head is a value (`$f x`, `{ |x| body } x`, a bound
    /// name).  It carries no redirects, those being a shell effect and not a
    /// property of application; trailing ones become a
    /// [`CompKind::Redirect`] around it.
    App {
        head: Arc<Comp<P>>,
        args: Box<[Spanned<Val<P>>]>,
        #[serde(skip)]
        hole: P::Call,
    },
    /// M to p. N: run `comp`, bind its result, continue with `rest`.
    Bind {
        comp: Arc<Comp<P>>,
        #[serde(skip)]
        hole: P::Bind,
        pattern: Arc<Pattern>,
        rest: Arc<Comp<P>>,
    },
    /// M ⨟ N: run `first`, discard its result, continue with `rest`.
    Seq {
        first: Arc<Comp<P>>,
        rest: Arc<Comp<P>>,
    },
    /// The `index`-th member of a recursive group: `x⃗ : U C⃗ ⊢ Mᵢ : Cᵢ`, and the
    /// node has type `C_index`. A group of one is Levy's `rec x. M`.
    Rec {
        group: Arc<GroupNode<P>>,
        index: usize,
    },
    /// `if V T E` with `V : Bool` and `T, E : U C`; the chosen arm is forced.
    /// Every form that suspends a command takes a thunk, so an arm is a literal
    /// block or a thunk in hand.
    If {
        cond: Spanned<Val<P>>,
        then: Spanned<Val<P>>,
        else_: Spanned<Val<P>>,
    },
    /// Levy's sum eliminator: one arm per tag, each a thunk of a function of
    /// the payload.  The alternatives are a finite list fixed at parse time, so
    /// exhaustiveness is always decided statically.
    Case {
        scrutinee: Spanned<Val<P>>,
        arms: Vec<CaseArm<P>>,
    },
    /// `try BODY HANDLER` — catch an error out of `body` and pass it to
    /// `handler`, a thunk of one argument.
    Try {
        body: Val<P>,
        handler: Val<P>,
    },
    /// Shield `acquire`, then install the release before calling `body resource`.
    /// Shield `release resource exit_case`; preserve the body's outcome unless release halts.
    Guard {
        acquire: Val<P>,
        body: Val<P>,
        release: Val<P>,
    },
    /// Checker-inserted value boundary: run `body` with its byte channel
    /// captured and hand those bytes over exactly, as `Bytes`. Total, and
    /// lossless. No surface syntax.
    Capture(Arc<Comp<P>>),
    Prim(Prim<P>),
    // Shell layer.
    /// Shell command invocation, and the effect boundary — nothing outside
    /// this variant reaches the dispatch chain or an external program.
    /// Redirects live on the call, not around it: they are installed once the
    /// head is admitted, and the capture wrap goes around them.
    Exec(Exec<P>),
    /// Two or more concurrent stages: stdout of stage N feeds stdin of stage N+1.
    Pipeline {
        stages: Vec<Arc<Comp<P>>>,
    },
    /// Redirect frame for a body that cannot fuse its own redirects — a
    /// CBPV `App`, or a nested effect frame.  `body` is an `Arc<Comp>` and
    /// not a thunk-shaped `Val`, so the invoke arm needs no runtime fallback.
    Redirect {
        body: Arc<Comp<P>>,
        redirects: Redirects<Val<P>>,
    },
    /// `within OPTS BODY` — install the option overrides in `opts`, written
    /// out pair by pair, for the duration of `body`.  `handlers` is not
    /// among them: its labels are the names it binds in `body`, so the arms
    /// are syntax and `None` is a `within` that wrote none.
    Within {
        opts: Arc<FieldsNode<P>>,
        handlers: Option<Vec<HandlerArmV<P>>>,
        body: Val<P>,
    },
    /// `grant CAPS BODY` — attenuate the active capability set across `body`.
    Grant {
        caps: Arc<FieldsNode<P>>,
        body: Val<P>,
    },
    /// `spawn [TITLE] BODY` — run `body` on a worker of its own and return its
    /// handle; the title names it to people.
    Spawn {
        title: Option<Spanned<Val<P>>>,
        body: Val<P>,
    },
    /// `gather BODY` — run `body`, owning the workers spawned inside it:
    /// they are joined at its return and ended at its halt.
    Gather {
        body: Val<P>,
    },
    /// `audit BODY` — record an audit subtree over `body` and reify it as a
    /// `[outcome: `ok v | `err e, trail]` record.
    Audit {
        body: Val<P>,
    },
    /// A `~` or `~/x` in value position: the home directory with the suffix
    /// appended. Never a value, since reading the store is an effect.
    Tilde(TildePath),
    /// A runtime membrane's check of what `body` returns.
    Protect {
        body: Arc<Comp<P>>,
        site: Arc<Site>,
    },
    /// A bare head bound in scope, called with `args` while the checker
    /// decides its splits; readback makes it an [`Exec`]. The only skipped
    /// variant, kept last so it does not shift any wire index.
    #[serde(skip)]
    BindingCall {
        head: Name,
        args: Box<[Spanned<Val<P>>]>,
        redirects: Redirects<Val<P>>,
        hole: P::BindingCall,
    },
}

/// A fixed operation on values: no name is looked up and nothing runs inside it;
/// it may fail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound = "P: Serial")]
pub enum Prim<P: Phase = Checked> {
    /// Arithmetic, comparison, or equality.
    Binary(BinaryOp, Val<P>, Val<P>),
    /// Numeric negation, without choosing a zero's type.
    Negate(Val<P>),
    Not(Val<P>),
    /// Scalar rendering for interpolation.
    Render(Val<P>),
    Concat(Box<[Val<P>]>),
    /// Drop one trailing line terminator, then decode strict UTF-8.
    Decode(Val<P>),
    Index {
        target: Val<P>,
        key: Spanned<Key<P>>,
    },
    Update {
        base: Spanned<Val<P>>,
        fields: Box<[(Name, Spanned<Val<P>>)]>,
    },
    List(Box<[ValListElem<P>]>),
    Map(Box<[MapEntryV<P>]>),
}

impl<P: Phase> Prim<P> {
    pub fn operands(&self) -> impl Iterator<Item = &Val<P>> {
        let mut operands = Vec::new();
        match self {
            Self::Binary(_, lhs, rhs) => operands.extend([lhs, rhs]),
            Self::Negate(value) | Self::Not(value) | Self::Render(value) | Self::Decode(value) => {
                operands.push(value);
            }
            Self::Concat(parts) => operands.extend(parts.iter()),
            Self::Index { target, key } => {
                operands.push(target);
                if let Key::Data(value) = &key.item {
                    operands.push(value);
                }
            }
            Self::Update { base, fields } => {
                operands.push(&base.item);
                operands.extend(fields.iter().map(|(_, value)| &value.item));
            }
            Self::List(elems) => operands.extend(elems.iter().map(|elem| &elem.slot().item)),
            Self::Map(entries) => {
                for entry in entries {
                    match entry {
                        MapEntryV::Entry(key, value) => operands.extend([&key.item, &value.item]),
                        MapEntryV::Spread(value) => operands.push(&value.item),
                    }
                }
            }
        }
        operands.into_iter()
    }

    pub(crate) fn map_operands<T: Phase>(
        &self,
        mut f: impl FnMut(&Val<P>, Option<Span>) -> Val<T>,
    ) -> Prim<T> {
        let mut spanned =
            |value: &Spanned<Val<P>>| Spanned::with_span(value.span, f(&value.item, value.span));
        match self {
            Self::Binary(op, lhs, rhs) => Prim::Binary(*op, f(lhs, None), f(rhs, None)),
            Self::Negate(value) => Prim::Negate(f(value, None)),
            Self::Not(value) => Prim::Not(f(value, None)),
            Self::Render(value) => Prim::Render(f(value, None)),
            Self::Concat(parts) => Prim::Concat(parts.iter().map(|value| f(value, None)).collect()),
            Self::Decode(value) => Prim::Decode(f(value, None)),
            Self::Index { target, key } => Prim::Index {
                target: f(target, None),
                key: Spanned::with_span(
                    key.span,
                    match &key.item {
                        Key::Data(value) => Key::Data(f(value, None)),
                        Key::Label(label) => Key::Label(label.clone()),
                        Key::Position(index) => Key::Position(*index),
                    },
                ),
            },
            Self::Update { base, fields } => Prim::Update {
                base: spanned(base),
                fields: fields
                    .iter()
                    .map(|(name, value)| (name.clone(), spanned(value)))
                    .collect(),
            },
            Self::List(elems) => Prim::List(
                elems
                    .iter()
                    .map(|elem| match elem {
                        ValListElem::Single(value) => ValListElem::Single(spanned(value)),
                        ValListElem::Spread(value) => ValListElem::Spread(spanned(value)),
                    })
                    .collect(),
            ),
            Self::Map(entries) => Prim::Map(
                entries
                    .iter()
                    .map(|entry| match entry {
                        MapEntryV::Entry(key, value) => {
                            MapEntryV::Entry(spanned(key), spanned(value))
                        }
                        MapEntryV::Spread(value) => MapEntryV::Spread(spanned(value)),
                    })
                    .collect(),
            ),
        }
    }
}

/// One arm of a `within [handlers: …]`: the command name it stands in for,
/// and the value installed under it.  Arms are syntax, as [`CaseArm`]s are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound = "P: Serial")]
pub struct HandlerArmV<P: Phase = Checked> {
    pub name: String,
    pub value: Spanned<Val<P>>,
}

/// One arm of a [`CompKind::Case`]: a tag and a thunk of a function of its
/// payload — a literal `{ |p| … }`, or a thunk in hand.
///
/// The label is stored bare, as [`Val::Variant`] stores it, so matching is a
/// string comparison and not a row-label round trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound = "P: Serial")]
pub struct CaseArm<P: Phase = Checked> {
    pub tag: Spanned<String>,
    pub(crate) body: Spanned<Val<P>>,
}

/// Body of a [`CompKind::Exec`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound = "P: Serial")]
pub struct Exec<P: Phase = Checked> {
    pub(crate) head: CommandWord,
    /// Each element crosses rendered, whichever boundary it reaches — a
    /// handler arm, a base frame, or the syscall itself.
    pub(crate) argv: Argv<P>,
    pub(crate) redirects: Redirects<Val<P>>,
    /// The type the checker solved at a boundary call, which its door admits
    /// the value it lets in against.  Unchecked, `None` everywhere; checked,
    /// `None` means no boundary head, since that is decided by name resolution
    /// at run time.
    pub(crate) site: Option<P::Boundary>,
}

/// Dispatch shape of an [`Exec`] head — a variant rather than a flag on
/// `Name`, so the IR shape carries the decision instead of burying it in a
/// boolean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum CommandWord {
    /// Resolved at evaluation time: env, then handlers, then PATH.
    Name(CommandName),
    /// `^name` — like a path head, the external of that spelling directly:
    /// skips the env, every native, and the handler stack alike.
    External(CommandName),
}

impl CommandWord {
    /// The head name, common to both variants.
    pub fn name(&self) -> &CommandName {
        match self {
            Self::Name(n) | Self::External(n) => n,
        }
    }
}

/// An `Occ` naming exactly `names`, for tests elsewhere in the crate that
/// need one without elaborating a program: a list node's occ *is* its
/// sorted, distinct set of mentioned variables.
#[cfg(test)]
pub(crate) fn test_occ(names: &[&str]) -> Occ {
    let elems: Box<[Spanned<Val>]> = names
        .iter()
        .map(|n| Spanned::synthetic(Val::Variable(Arc::from(*n))))
        .collect();
    ListNode::new(elems).occ().clone()
}

#[cfg(test)]
mod tests;
