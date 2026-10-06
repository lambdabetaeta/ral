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
pub use pattern::{MapPatternEntry, Pattern};
pub use redirect::{Redirect, Redirects, StderrTarget, StdinSource, WriteMode};

use crate::first_order::Finite;
use crate::path::tilde::TildePath;
use crate::source::Spanned;
use crate::text::Str;

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

pub type ThunkNode = Node<Arc<Comp>>;
pub type GroupNode = Node<Box<[(Name, Arc<Comp>)]>>;
pub type ListNode = Node<Box<[Spanned<Val>]>>;
/// Entries sorted by key, stably; equal keys are the checker's to refuse.
pub type FieldsNode = Node<Box<[(Name, Spanned<Val>)]>>;
/// A form's written options, in source order: labels are syntax.
pub type OptionsV = Box<[(Name, Spanned<Val>)]>;

impl Mentions for Arc<Comp> {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        self.as_ref().mentions(out);
    }
}

impl Mentions for Box<[(Name, Arc<Comp>)]> {
    // Every member's: the group is one unit.
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        for (_name, member) in self {
            member.mentions(out);
        }
    }
}

impl Mentions for Box<[Spanned<Val>]> {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        for elem in self {
            elem.item.mentions(out);
        }
    }
}

impl Mentions for Box<[(Name, Spanned<Val>)]> {
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
    /// Slash-bearing literal path: skips the lookup chain, exec'd as written.
    Path(String),
    /// `~/x`, carried unexpanded until command resolution.
    TildePath(TildePath),
}

impl CommandName {
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
pub enum Val {
    Unit,
    String(Str),
    Int(i64),
    Float(Finite),
    Bool(bool),
    Variable(Name),
    /// A suspended computation, eliminated by [`CompKind::Force`]: `⟨M,
    /// ρ|occ(M)⟩`.
    Thunk(Arc<ThunkNode>),
    /// A plain list literal: no spread, so every element is here already.
    List(Arc<ListNode>),
    /// `[key: val, …]`: static labels, no spread.
    Record(Arc<FieldsNode>),
    /// `[:, key: val, …]`: static labels, no spread.
    Map(Arc<FieldsNode>),
    /// `` `label `` or `` `label payload ``; the label is stored without
    /// its leading backtick.
    Variant {
        label: Name,
        payload: Option<Box<Self>>,
    },
}

impl Val {
    /// `thunk M`, from an already-built `M`.
    pub(crate) fn thunk(comp: Arc<Comp>) -> Self {
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
pub enum ValListElem {
    Single(Spanned<Val>),
    /// `...x`, spliced into the surrounding list, its `...` included in an argument span.
    Spread(Spanned<Val>),
}

impl ValListElem {
    /// The slot's value, whichever form the slot takes.
    pub(crate) fn slot(&self) -> &Spanned<Val> {
        match self {
            Self::Single(v) | Self::Spread(v) => v,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ValRecordEntry {
    /// `label: value`; the label is the row's, tags included.
    Field(String, Spanned<Val>),
    /// `...x`, merged into the surrounding record.
    Spread(Spanned<Val>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ValMapEntry {
    /// `key: value`, the key a value evaluated to a `String` at run time.
    Entry(Val, Spanned<Val>),
    /// `...x`, merged into the surrounding map.
    Spread(Spanned<Val>),
}

/// A collection literal that a spread or a computed key keeps out of value
/// syntax: building it costs O(data) and can fail, so it is
/// [`CompKind::Assemble`], a primitive computation rather than a `Val`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Assembly {
    List(Vec<ValListElem>),
    Record(Vec<ValRecordEntry>),
    Map(Vec<ValMapEntry>),
}

/// One entry of either literal as the shared runtime carrier reads it.
#[derive(Clone, Copy)]
pub(crate) enum MapPart<'a> {
    Labelled(&'a str, &'a Spanned<Val>),
    Computed(&'a Val, &'a Spanned<Val>),
    Spread(&'a Spanned<Val>),
}

pub(crate) trait MapParts {
    fn part(&self) -> MapPart<'_>;
}

impl MapParts for ValRecordEntry {
    fn part(&self) -> MapPart<'_> {
        match self {
            Self::Field(label, value) => MapPart::Labelled(label, value),
            Self::Spread(value) => MapPart::Spread(value),
        }
    }
}

impl MapParts for ValMapEntry {
    fn part(&self) -> MapPart<'_> {
        match self {
            Self::Entry(key, value) => MapPart::Computed(key, value),
            Self::Spread(value) => MapPart::Spread(value),
        }
    }
}

/// Positional arguments to a call — the same slots a list literal has.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Args(Vec<ValListElem>);

impl Args {
    /// The args as a literal positional list, or `None` if any element is a
    /// `Spread` — dynamic arity, so callers fall back to weaker checks.
    pub(crate) fn positional(&self) -> Option<Vec<&Val>> {
        self.0
            .iter()
            .map(|e| match e {
                ValListElem::Single(v) => Some(&v.item),
                ValListElem::Spread(_) => None,
            })
            .collect()
    }

    /// How many arguments there are, unless a spread makes that dynamic.
    pub(crate) fn arity(&self) -> Option<usize> {
        self.positional().map(|p| p.len())
    }
}

impl std::ops::Deref for Args {
    type Target = [ValListElem];
    fn deref(&self) -> &[ValListElem] {
        &self.0
    }
}

impl From<Vec<ValListElem>> for Args {
    fn from(elems: Vec<ValListElem>) -> Self {
        Self(elems)
    }
}

impl FromIterator<ValListElem> for Args {
    fn from_iter<I: IntoIterator<Item = ValListElem>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl Extend<ValListElem> for Args {
    fn extend<I: IntoIterator<Item = ValListElem>>(&mut self, iter: I) {
        self.0.extend(iter);
    }
}

impl IntoIterator for Args {
    type Item = ValListElem;
    type IntoIter = std::vec::IntoIter<ValListElem>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a Args {
    type Item = &'a ValListElem;
    type IntoIter = std::slice::Iter<'a, ValListElem>;
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

/// One top-level statement, checked unless `S` says it is not: the schemes
/// are the checker's, so elaboration holds none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Phrase<S = DefineSchemes> {
    /// `let p = M` at the top level: run M, extend the session environment.
    /// `schemes` has one entry per name `pattern` binds, closed and
    /// generalised by the checker — a destructuring `let` carries one per
    /// component.
    Define {
        pattern: Arc<Pattern>,
        comp: Arc<Comp>,
        schemes: S,
    },
    /// Any other statement.
    Run(Arc<Comp>),
}

/// What elaboration yields, and the checker alone turns into a [`Toplevel`].
pub type Unchecked = Vec<Spanned<Phrase<()>>>;

impl CompKind {
    /// `comp` to `pattern`, then `rest`.
    pub(crate) fn bind(
        pattern: Pattern,
        comp: impl Into<Arc<Comp>>,
        rest: impl Into<Arc<Comp>>,
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
pub type Comp = Spanned<CompKind>;

impl Comp {
    /// `Some` for a `Lam`, and for a `Rec` projection whose member is a
    /// `Lam`: the checker's η-expansion guarantees the body of every
    /// function-typed thunk *is* a `Lam`, so reading the shape here is
    /// reading the type.
    pub fn arrow(&self) -> Option<(&Pattern, &Arc<Self>)> {
        match &self.item {
            CompKind::Lam { param, body } => Some((param, body)),
            CompKind::Rec { group, index } => group.shape()[*index].1.arrow(),
            _ => None,
        }
    }
}

impl Mentions for Comp {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        match &self.item {
            CompKind::Lam { param: _, body } | CompKind::Capture(body) => body.mentions(out),
            CompKind::Bind {
                comp,
                pattern: _,
                rest,
            } => {
                comp.mentions(out);
                rest.mentions(out);
            }
            CompKind::App { head, args } => {
                head.mentions(out);
                args.mentions(out);
            }
            CompKind::Exec(exec) => {
                exec.head.mentions(out);
                exec.args.mentions(out);
                exec.redirects.mentions(out);
            }
            CompKind::Pipeline { stages } => {
                for stage in stages {
                    stage.mentions(out);
                }
            }
            CompKind::Binary(_op, a, b) => {
                a.mentions(out);
                b.mentions(out);
            }
            CompKind::Force(v) | CompKind::Return(v) | CompKind::Negate(v) | CompKind::Not(v) => {
                v.mentions(out);
            }
            CompKind::Assemble(assembly) => assembly.mentions(out),
            CompKind::Index { target, keys } => {
                target.mentions(out);
                for key in keys {
                    key.item.mentions(out);
                }
            }
            CompKind::Interpolation(vals) => {
                for v in vals {
                    v.mentions(out);
                }
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
                opts.mentions(out);
                for arm in handlers.iter().flatten() {
                    arm.value.item.mentions(out);
                }
                body.mentions(out);
            }
            CompKind::Grant { caps, body } => {
                caps.mentions(out);
                body.mentions(out);
            }
            CompKind::Audit { body } => body.mentions(out),
            CompKind::Redirect { body, redirects } => {
                body.mentions(out);
                redirects.mentions(out);
            }
            CompKind::Decode(val) => val.mentions(out),
        }
    }
}

impl Mentions for Val {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        match self {
            Self::Unit | Self::String(_) | Self::Int(_) | Self::Float(_) | Self::Bool(_) => {}
            Self::Variable(name) => out.push(name),
            Self::Thunk(node) => out.extend(node.occ().names()),
            Self::List(node) => out.extend(node.occ().names()),
            Self::Record(node) | Self::Map(node) => out.extend(node.occ().names()),
            Self::Variant { label: _, payload } => {
                if let Some(p) = payload {
                    p.mentions(out);
                }
            }
        }
    }
}

impl<'a> MapPart<'a> {
    fn mentions(self, out: &mut Vec<&'a Name>) {
        match self {
            Self::Labelled(_, value) | Self::Spread(value) => value.item.mentions(out),
            Self::Computed(key, value) => {
                key.mentions(out);
                value.item.mentions(out);
            }
        }
    }
}

impl Mentions for Assembly {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        match self {
            Self::List(elems) => elems.mentions(out),
            Self::Record(entries) => {
                for entry in entries {
                    entry.part().mentions(out);
                }
            }
            Self::Map(entries) => {
                for entry in entries {
                    entry.part().mentions(out);
                }
            }
        }
    }
}

impl Mentions for [ValListElem] {
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

impl Mentions for Redirects<Val> {
    fn mentions<'a>(&'a self, out: &mut Vec<&'a Name>) {
        for v in self.operands() {
            v.mentions(out);
        }
    }
}

/// The CBPV computation category — the evaluator steps a program by
/// matching on this.  Variant docs use Levy's CBPV notation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CompKind {
    /// force V — run a thunk.
    Force(Val),
    /// λ — evaluates to a closure.
    Lam { param: Pattern, body: Arc<Comp> },
    /// return V — produce a value.
    Return(Val),
    /// Build a list, record, or map some of whose parts are spread or keyed
    /// at run time — the one rule that does O(data) work over value syntax.
    Assemble(Assembly),
    /// M to x. N — run `comp`, bind its result, continue with `rest`.  `a; b`
    /// is `a to _. b`, on `Wildcard`.
    Bind {
        comp: Arc<Comp>,
        pattern: Arc<Pattern>,
        rest: Arc<Comp>,
    },
    /// `M : A → B, V : A ⊢ M V : B` — the elimination form taken when the
    /// head resolves to a bound value (`$f x`, `(|x| body) x`).  It carries
    /// no redirects, those being a shell effect and not a property of
    /// application; trailing ones become a [`CompKind::Redirect`] around it.
    App { head: Arc<Comp>, args: Args },
    /// Shell command invocation, and the effect boundary — nothing outside
    /// this variant reaches the dispatch chain or an external program.
    /// Redirects live on the call, not around it: they are installed once the
    /// head is admitted, and the capture wrap goes around them.
    Exec(Exec),
    /// Concurrent stages joined by Unix pipes: stdout of stage N feeds
    /// stdin of stage N+1.
    Pipeline { stages: Vec<Arc<Comp>> },
    /// Binary primitive on already-evaluated values (`$[a + b]`, `$[a == b]`).
    Binary(BinaryOp, Val, Val),
    /// `-v` on a number.  Its own variant rather than a subtraction from a
    /// literal zero, which would have to pick that zero's type and so force
    /// the operand to match it — negating a `Float` would not typecheck.
    Negate(Val),
    /// `not v` on a `Bool`.  Its own variant so the IR cannot spell a
    /// two-operand `not` or a one-operand `Add`; evaluator and typechecker
    /// dispatch on the tag rather than a runtime arity guard.
    Not(Val),
    /// `V[k1][k2]` — computation-typed only because it can fail (key not
    /// found, out of bounds); target and keys are themselves pure.
    Index {
        target: Val,
        keys: Vec<Spanned<Val>>,
    },
    /// String interpolation, effectful because a lookup can fail.
    Interpolation(Vec<Val>),
    /// The `index`-th member of a recursive group: `x⃗ : U C⃗ ⊢ Mᵢ : Cᵢ`, and the
    /// node has type `C_index`. A group of one is Levy's `rec x. M`.
    Rec { group: Arc<GroupNode>, index: usize },
    /// A `~` or `~/x` in value position: the home directory with the suffix
    /// appended. Never a value, since reading the store is an effect.
    Tilde(TildePath),
    /// `if V T E` with `V : Bool` and `T, E : U C`; the chosen arm is forced.
    /// Every form that suspends a command takes a thunk, so an arm is a literal
    /// block or a thunk in hand.
    If {
        cond: Spanned<Val>,
        then: Spanned<Val>,
        else_: Spanned<Val>,
    },
    /// Levy's sum eliminator: one arm per tag, each a thunk of a function of
    /// the payload.  The alternatives are a finite list fixed at parse time, so
    /// exhaustiveness is always decided statically.
    Case {
        scrutinee: Spanned<Val>,
        arms: Vec<CaseArm>,
    },
    /// `try BODY HANDLER` — catch an error out of `body` and pass it to
    /// `handler`, a thunk of one argument.
    Try { body: Val, handler: Val },
    /// `guard BODY CLEANUP` — `cleanup` runs unconditionally; a failure in
    /// it is reported but does not mask the body's result.
    Guard { body: Val, cleanup: Val },
    /// `audit BODY` — record an audit subtree over `body` and reify it as a
    /// `[outcome: `ok v | `err e, trail]` record.
    Audit { body: Val },
    /// `within OPTS BODY` — install the option overrides in `opts`, written
    /// out pair by pair, for the duration of `body`.  `handlers` is not
    /// among them: its labels are the names it binds in `body`, so the arms
    /// are syntax and `None` is a `within` that wrote none.
    Within {
        opts: OptionsV,
        handlers: Option<Vec<HandlerArmV>>,
        body: Val,
    },
    /// `grant CAPS BODY` — attenuate the active capability set across `body`.
    Grant { caps: OptionsV, body: Val },
    /// Redirect frame for a body that cannot fuse its own redirects — a
    /// CBPV `App`, or a nested effect frame.  `body` is an `Arc<Comp>` and
    /// not a thunk-shaped `Val`, so the invoke arm needs no runtime fallback.
    Redirect {
        body: Arc<Comp>,
        redirects: Redirects<Val>,
    },
    /// Checker-inserted value boundary: run `body` with its byte channel
    /// captured and hand those bytes over exactly, as `Bytes`. Total, and
    /// lossless. No surface syntax.
    Capture(Arc<Comp>),
    /// Checker-inserted coercion: read the `Bytes` value as text — one
    /// trailing line terminator dropped, then a strict UTF-8 decode. The
    /// partial, lossy half of the value boundary, kept its own node so that
    /// [`CompKind::Capture`] stays exact. The kernel's `decode` takes a
    /// value, so the checker reaches it through a bind: `Bind(Capture(M), x,
    /// Decode(x))`.
    ///
    /// It is syntax and not a command because its meaning is fixed where the
    /// checker writes it: no name is looked up, and no frame the user can
    /// install stands between the bytes and their reading. No surface syntax.
    Decode(Val),
}

/// One arm of a `within [handlers: …]`: the command name it stands in for,
/// and the value installed under it.  Arms are syntax, as [`CaseArm`]s are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandlerArmV {
    pub name: String,
    pub value: Spanned<Val>,
}

/// One arm of a [`CompKind::Case`]: a tag and a thunk of a function of its
/// payload — a literal `{ |p| … }`, or a thunk in hand.
///
/// The label is stored bare, as [`Val::Variant`] stores it, so matching is a
/// string comparison and not a row-label round trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseArm {
    pub tag: Spanned<String>,
    pub(crate) body: Spanned<Val>,
}

/// Body of a [`CompKind::Exec`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exec {
    pub(crate) head: CommandWord,
    /// An argv: each element crosses rendered, whichever boundary it reaches —
    /// a handler arm, a base frame, or the syscall itself.
    pub(crate) args: Args,
    pub(crate) redirects: Redirects<Val>,
    /// The type the checker solved at a boundary call, which its door admits
    /// the value it lets in against.  Unchecked, `None` everywhere; checked,
    /// `None` means no boundary head, since that is decided by name resolution
    /// at run time.
    pub(crate) site: Option<Arc<crate::ty::Site>>,
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
