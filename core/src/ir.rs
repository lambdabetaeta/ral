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
use crate::path::tilde::TildePath;
use crate::source::{Span, Spanned};
use crate::ty::Site;

// ── Values ──────────────────────────────────────────────────────────────
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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

/// Every name a node mentions, bound or free: sorted, distinct. Built only by
/// [`Node::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occ(Arc<[Name]>);

impl Occ {
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
        let mut refs = Vec::new();
        shape.mentions(&mut refs);
        refs.sort_unstable();
        refs.dedup();
        let occ = Occ(refs.into_iter().cloned().collect());
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

pub type ThunkNode<S = Arc<Site>> = Node<Arc<Comp<S>>>;
pub type GroupNode<S = Arc<Site>> = Node<Box<[(Name, Arc<Comp<S>>)]>>;
pub type ListNode<S = Arc<Site>> = Node<Box<[Spanned<Val<S>>]>>;
/// Entries sorted by key, stably; equal keys are the checker's to refuse.
pub type FieldsNode<S = Arc<Site>> = Node<Box<[(Name, Spanned<Val<S>>)]>>;

impl<S> Mentions for Arc<Comp<S>> {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        self.as_ref().mentions(out);
    }
}

impl<S> Mentions for Box<[(Name, Arc<Comp<S>>)]> {
    // Every member's: the group is one unit.
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        for (_name, member) in self {
            member.mentions(out);
        }
    }
}

impl<S> Mentions for Box<[Spanned<Val<S>>]> {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        for elem in self {
            elem.item.mentions(out);
        }
    }
}

impl<S> Mentions for Box<[(Name, Spanned<Val<S>>)]> {
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
pub enum Val<S = Arc<Site>> {
    Unit,
    String(Str),
    Int(i64),
    Float(Finite),
    Bool(bool),
    Variable(Name),
    /// A suspended computation, eliminated by [`CompKind::Force`]: `⟨M,
    /// ρ|occ(M)⟩`.
    Thunk(Arc<ThunkNode<S>>),
    /// A plain list literal: no spread, so every element is here already.
    List(Arc<ListNode<S>>),
    /// `[key: val, …]`: static labels, no spread.
    Record(Arc<FieldsNode<S>>),
    /// `[:, key: val, …]`: keys written out, no spread.
    Map(Arc<FieldsNode<S>>),
    /// The label has no leading backtick; a bare tag carries `Unit`.
    Variant {
        label: Name,
        payload: Box<Self>,
    },
    /// Checker-inserted protection at an instantiation site.
    Protected {
        value: Box<Self>,
        site: S,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Key<S = Arc<Site>> {
    Label(Str),
    Position(usize),
    Data(Val<S>),
}

impl<S> Val<S> {
    /// `thunk M`, from an already-built `M`.
    pub(crate) fn thunk(comp: Arc<Comp<S>>) -> Self {
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
pub enum ValListElem<S = Arc<Site>> {
    Single(Spanned<Val<S>>),
    /// `...x`, spliced into the surrounding list, its `...` included in an argument span.
    Spread(Spanned<Val<S>>),
}

impl<S> ValListElem<S> {
    /// The slot's value, whichever form the slot takes.
    pub(crate) fn slot(&self) -> &Spanned<Val<S>> {
        match self {
            Self::Single(v) | Self::Spread(v) => v,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MapEntryV<S = Arc<Site>> {
    Entry(Spanned<Val<S>>, Spanned<Val<S>>),
    Spread(Spanned<Val<S>>),
}

/// A command's argv — the same slots a list literal has, a `...` splicing a
/// list in place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Argv<S = Arc<Site>>(Vec<ValListElem<S>>);

impl<S> Default for Argv<S> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<S> Argv<S> {
    /// The slots as a literal positional list, or `None` if any is a
    /// `Spread` — dynamic arity, so callers fall back to weaker checks.
    pub(crate) fn positional(&self) -> Option<Vec<&Spanned<Val<S>>>> {
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

impl<S> std::ops::Deref for Argv<S> {
    type Target = [ValListElem<S>];
    fn deref(&self) -> &[ValListElem<S>] {
        &self.0
    }
}

impl<S> From<Vec<ValListElem<S>>> for Argv<S> {
    fn from(elems: Vec<ValListElem<S>>) -> Self {
        Self(elems)
    }
}

impl<S> FromIterator<ValListElem<S>> for Argv<S> {
    fn from_iter<I: IntoIterator<Item = ValListElem<S>>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl<S> Extend<ValListElem<S>> for Argv<S> {
    fn extend<I: IntoIterator<Item = ValListElem<S>>>(&mut self, iter: I) {
        self.0.extend(iter);
    }
}

impl<S> IntoIterator for Argv<S> {
    type Item = ValListElem<S>;
    type IntoIter = std::vec::IntoIter<ValListElem<S>>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a, S> IntoIterator for &'a Argv<S> {
    type Item = &'a ValListElem<S>;
    type IntoIter = std::slice::Iter<'a, ValListElem<S>>;
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
pub type DefineSchemes = Vec<(String, Arc<crate::ty::Scheme>)>;

/// One top-level statement, checked unless `D` says it is not: the schemes
/// are the checker's, so elaboration holds none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Phrase<D = DefineSchemes, S = Arc<Site>> {
    /// `let p = M` at the top level: run M, extend the session environment.
    /// `schemes` has one entry per name `pattern` binds, closed and
    /// generalised by the checker — a destructuring `let` carries one per
    /// component.
    Define {
        pattern: Arc<Pattern>,
        comp: Arc<Comp<S>>,
        schemes: D,
    },
    /// Any other statement.
    Run(Arc<Comp<S>>),
}

/// What elaboration yields, and the checker alone turns into a [`Toplevel`].
pub type Unchecked = Vec<Spanned<Phrase<()>>>;

impl<S> CompKind<S> {
    /// `comp` to `pattern`, then `rest`.
    pub(crate) fn bind(
        pattern: Pattern,
        comp: impl Into<Arc<Comp<S>>>,
        rest: impl Into<Arc<Comp<S>>>,
    ) -> Self {
        Self::Bind {
            comp: comp.into(),
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
    pub(crate) fn exported_schemes(&self) -> Vec<(String, Arc<crate::ty::Scheme>)> {
        let last: std::collections::BTreeMap<_, _> = self
            .phrases
            .iter()
            .filter_map(|phrase| match &phrase.item {
                Phrase::Define { schemes, .. } => Some(schemes),
                Phrase::Run(_) => None,
            })
            .flatten()
            .map(|(name, scheme)| (name.clone(), Arc::clone(scheme)))
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
pub type Comp<S = Arc<Site>> = Spanned<CompKind<S>>;

impl<S> Comp<S> {
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

    /// The command this computation is, looking through the checker's value
    /// boundary: `Capture`, then its `Decode`.
    pub(crate) fn command(&self) -> Option<&Exec<S>> {
        match &self.item {
            CompKind::Exec(exec) => Some(exec),
            CompKind::Capture(body) => body.command(),
            CompKind::Bind { comp, rest, .. }
                if matches!(rest.item, CompKind::Prim(Prim::Decode(_))) =>
            {
                comp.command()
            }
            _ => None,
        }
    }
}

impl<S> Mentions for Comp<S> {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        match &self.item {
            CompKind::Lam { param: _, body }
            | CompKind::Capture(body)
            | CompKind::Protect { body, .. } => body.mentions(out),
            CompKind::Bind {
                comp,
                pattern: _,
                rest,
            }
            | CompKind::Seq { first: comp, rest } => {
                comp.mentions(out);
                rest.mentions(out);
            }
            CompKind::App { head, args } => {
                head.mentions(out);
                args.mentions(out);
            }
            CompKind::Exec(exec) => {
                exec.head.mentions(out);
                exec.argv.mentions(out);
                exec.redirects.mentions(out);
            }
            CompKind::Native { entry: _, args } => {
                for arg in args {
                    arg.mentions(out);
                }
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
            CompKind::Guard { body, cleanup } => {
                body.mentions(out);
                cleanup.mentions(out);
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
        }
    }
}

impl<S> Mentions for Val<S> {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        match self {
            Self::Unit | Self::String(_) | Self::Int(_) | Self::Float(_) | Self::Bool(_) => {}
            Self::Variable(name) => out.push(name),
            Self::Protected { value, .. } => value.mentions(out),
            Self::Thunk(node) => out.extend(node.occ().names()),
            Self::List(node) => out.extend(node.occ().names()),
            Self::Record(node) | Self::Map(node) => out.extend(node.occ().names()),
            Self::Variant { payload, .. } => payload.mentions(out),
        }
    }
}

impl<S> Mentions for [ValListElem<S>] {
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

impl<S> Mentions for Redirects<Val<S>> {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        for v in self.operands() {
            v.mentions(out);
        }
    }
}

/// The CBPV computation category — the evaluator steps a program by
/// matching on this.  Variant docs use Levy's CBPV notation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CompKind<S = Arc<Site>> {
    // CBPV core.
    /// return V — produce a value.
    Return(Val<S>),
    /// force V — run a thunk.
    Force(Val<S>),
    /// λ — evaluates to a closure.
    Lam {
        param: Name,
        body: Arc<Comp<S>>,
    },
    /// `M V₁ … Vₙ`, `n ≥ 1`: the arguments are pushed, then `head` runs —
    /// taken when the head is a value (`$f x`, `{ |x| body } x`, a bound
    /// name).  It carries no redirects, those being a shell effect and not a
    /// property of application; trailing ones become a
    /// [`CompKind::Redirect`] around it.
    App {
        head: Arc<Comp<S>>,
        args: Box<[Spanned<Val<S>>]>,
    },
    /// M to p. N: run `comp`, bind its result, continue with `rest`.
    Bind {
        comp: Arc<Comp<S>>,
        pattern: Arc<Pattern>,
        rest: Arc<Comp<S>>,
    },
    /// M ⨟ N: run `first`, discard its result, continue with `rest`.
    Seq {
        first: Arc<Comp<S>>,
        rest: Arc<Comp<S>>,
    },
    /// The `index`-th member of a recursive group: `x⃗ : U C⃗ ⊢ Mᵢ : Cᵢ`, and the
    /// node has type `C_index`. A group of one is Levy's `rec x. M`.
    Rec {
        group: Arc<GroupNode<S>>,
        index: usize,
    },
    /// `if V T E` with `V : Bool` and `T, E : U C`; the chosen arm is forced.
    /// Every form that suspends a command takes a thunk, so an arm is a literal
    /// block or a thunk in hand.
    If {
        cond: Spanned<Val<S>>,
        then: Spanned<Val<S>>,
        else_: Spanned<Val<S>>,
    },
    /// Levy's sum eliminator: one arm per tag, each a thunk of a function of
    /// the payload.  The alternatives are a finite list fixed at parse time, so
    /// exhaustiveness is always decided statically.
    Case {
        scrutinee: Spanned<Val<S>>,
        arms: Vec<CaseArm<S>>,
    },
    /// `try BODY HANDLER` — catch an error out of `body` and pass it to
    /// `handler`, a thunk of one argument.
    Try {
        body: Val<S>,
        handler: Val<S>,
    },
    /// `guard BODY CLEANUP` — `cleanup` runs unconditionally; a failure in
    /// it replaces the body's outcome.
    Guard {
        body: Val<S>,
        cleanup: Val<S>,
    },
    /// Checker-inserted value boundary: run `body` with its byte channel
    /// captured and hand those bytes over exactly, as `Bytes`. Total, and
    /// lossless. No surface syntax.
    Capture(Arc<Comp<S>>),
    Prim(Prim<S>),
    // Shell layer.
    /// Shell command invocation, and the effect boundary — nothing outside
    /// this variant reaches the dispatch chain or an external program.
    /// Redirects live on the call, not around it: they are installed once the
    /// head is admitted, and the capture wrap goes around them.
    Exec(Exec<S>),
    /// Two or more concurrent stages: stdout of stage N feeds stdin of stage N+1.
    Pipeline {
        stages: Vec<Arc<Comp<S>>>,
    },
    /// Redirect frame for a body that cannot fuse its own redirects — a
    /// CBPV `App`, or a nested effect frame.  `body` is an `Arc<Comp>` and
    /// not a thunk-shaped `Val`, so the invoke arm needs no runtime fallback.
    Redirect {
        body: Arc<Comp<S>>,
        redirects: Redirects<Val<S>>,
    },
    /// `within OPTS BODY` — install the option overrides in `opts`, written
    /// out pair by pair, for the duration of `body`.  `handlers` is not
    /// among them: its labels are the names it binds in `body`, so the arms
    /// are syntax and `None` is a `within` that wrote none.
    Within {
        opts: Arc<FieldsNode<S>>,
        handlers: Option<Vec<HandlerArmV<S>>>,
        body: Val<S>,
    },
    /// `grant CAPS BODY` — attenuate the active capability set across `body`.
    Grant {
        caps: Arc<FieldsNode<S>>,
        body: Val<S>,
    },
    /// `spawn [TITLE] BODY` — run `body` on a worker of its own and return its
    /// handle; the title names it to people.
    Spawn {
        title: Option<Spanned<Val<S>>>,
        body: Val<S>,
    },
    /// `gather BODY` — run `body`, owning the workers spawned inside it:
    /// they are joined at its return and ended at its halt.
    Gather {
        body: Val<S>,
    },
    /// `audit BODY` — record an audit subtree over `body` and reify it as a
    /// `[outcome: `ok v | `err e, trail]` record.
    Audit {
        body: Val<S>,
    },
    /// A `~` or `~/x` in value position: the home directory with the suffix
    /// appended. Never a value, since reading the store is an effect.
    Tilde(TildePath),
    /// Checker-inserted protection at an instantiation site.
    Protect {
        body: Arc<Comp<S>>,
        site: S,
    },
    /// A builtin run on the values `args` name: the body of a native's code,
    /// which forcing the native builds.  Never elaborated, so never serialised;
    /// last, since a skipped variant shifts the wire index of every one after.
    #[serde(skip)]
    Native {
        entry: NativeEntry,
        args: Box<[Val<S>]>,
    },
}

/// A fixed operation on values: no name is looked up and nothing runs inside it;
/// it may fail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Prim<S = Arc<Site>> {
    /// Arithmetic, comparison, or equality.
    Binary(BinaryOp, Val<S>, Val<S>),
    /// Numeric negation, without choosing a zero's type.
    Negate(Val<S>),
    Not(Val<S>),
    /// Scalar rendering for interpolation.
    Render(Val<S>),
    Concat(Box<[Val<S>]>),
    /// Drop one trailing line terminator, then decode strict UTF-8.
    Decode(Val<S>),
    Index {
        target: Val<S>,
        key: Spanned<Key<S>>,
    },
    Update {
        base: Spanned<Val<S>>,
        fields: Box<[(Name, Spanned<Val<S>>)]>,
    },
    List(Box<[ValListElem<S>]>),
    Map(Box<[MapEntryV<S>]>),
}

impl<S> Prim<S> {
    pub fn operands(&self) -> impl Iterator<Item = &Val<S>> {
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

    pub(crate) fn map_operands<T>(
        &self,
        mut f: impl FnMut(&Val<S>, Option<Span>) -> Val<T>,
    ) -> Prim<T> {
        let mut spanned =
            |value: &Spanned<Val<S>>| Spanned::with_span(value.span, f(&value.item, value.span));
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
pub struct HandlerArmV<S = Arc<Site>> {
    pub name: String,
    pub value: Spanned<Val<S>>,
}

/// One arm of a [`CompKind::Case`]: a tag and a thunk of a function of its
/// payload — a literal `{ |p| … }`, or a thunk in hand.
///
/// The label is stored bare, as [`Val::Variant`] stores it, so matching is a
/// string comparison and not a row-label round trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseArm<S = Arc<Site>> {
    pub tag: Spanned<String>,
    pub(crate) body: Spanned<Val<S>>,
}

/// A builtin as [`CompKind::Native`] holds it, equal by name:
/// [`BuiltinEntry`](crate::shell::runtime::BuiltinEntry) has no `PartialEq`.
#[derive(Debug, Clone)]
pub struct NativeEntry(pub(crate) Arc<crate::shell::runtime::BuiltinEntry>);

impl PartialEq for NativeEntry {
    fn eq(&self, other: &Self) -> bool {
        self.0.decl.name == other.0.decl.name
    }
}

/// Body of a [`CompKind::Exec`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exec<S = Arc<Site>> {
    pub(crate) head: CommandWord,
    /// Each element crosses rendered, whichever boundary it reaches — a
    /// handler arm, a base frame, or the syscall itself.
    pub(crate) argv: Argv<S>,
    pub(crate) redirects: Redirects<Val<S>>,
    /// The type the checker solved at a boundary call, which its door admits
    /// the value it lets in against.  Unchecked, `None` everywhere; checked,
    /// `None` means no boundary head, since that is decided by name resolution
    /// at run time.
    pub(crate) site: Option<S>,
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
