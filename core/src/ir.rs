//! Call-by-push-value intermediate representation: the target of
//! elaboration ([`crate::elaborator`]), the input to evaluation.
//!
//! [`Val`] is inert data and [`Comp`] is effectful, so a value can never
//! diverge or perform I/O.  The [`Spanned`] wrapper puts a source range on
//! every [`Comp`] and on each sub-`Val` position the typechecker narrows
//! onto, so a span rides with the value rather than the parent; `None`
//! means the node is synthetic — a builtin, the prelude, generated code.

use crate::path::tilde::TildePath;
use crate::source::Spanned;
use crate::syntax::ast::{BinaryOp, Pattern, Redirects};
use crate::types::Str;

/// A [`crate::syntax::ast::Pattern`] as elaboration hands it to the rest of
/// the IR — the same shape, under the IR's own name.
pub type IrPattern = Pattern;
pub(crate) type Param = IrPattern;

// ── Values ──────────────────────────────────────────────────────────────
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// An identifier the IR binds or mentions: a variable, a pattern name, a
/// bare command head, a variant label. Shared, not copied — a bind, a
/// capture and a label clone a pointer.
pub type Name = Arc<str>;

/// What the elaborator's hoisted temporaries are named after: `_var1`, ….
pub(crate) const GENSYM_PREFIX: &str = "_var";

pub(crate) fn is_gensym(name: &str) -> bool {
    name.starts_with(GENSYM_PREFIX)
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
    /// Tilde-headed path, carried unexpanded until command resolution.
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
    Float(f64),
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

    /// Classify a bare word into its most specific [`Val`] variant, by the
    /// shape rules of [`crate::syntax::ast::WordLiteral::classify`].
    ///
    /// Eager and type-blind: a numeric-looking word meant as argv data is
    /// read as a number, and stringifies back unchanged only where its
    /// source was already canonical (`007` ⇒ `7`, `1.50` ⇒ `1.5`).
    pub(crate) fn from_word(s: &str) -> Self {
        use crate::syntax::ast::WordLiteral;
        match WordLiteral::classify(s) {
            Some(WordLiteral::Bool(b)) => Self::Bool(b),
            Some(WordLiteral::Int(n)) => Self::Int(n),
            Some(WordLiteral::Float(f)) => Self::Float(f),
            None => Self::String(s.into()),
        }
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
pub(crate) type Args = Vec<ValListElem>;

/// Readers of an [`Args`].  Free functions, not methods — [`Args`] is a
/// type alias.
pub(crate) mod args {
    use super::{Args, Val, ValListElem};

    /// The args as a literal positional list, or `None` if any element is a
    /// `Spread` — dynamic arity, so callers fall back to weaker checks.
    pub(crate) fn positional(args: &Args) -> Option<Vec<&Val>> {
        let mut out = Vec::with_capacity(args.len());
        for e in args {
            match e {
                ValListElem::Single(v) => out.push(&v.item),
                ValListElem::Spread(_) => return None,
            }
        }
        Some(out)
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
    pub admits: Vec<(String, Arc<crate::types::Site>)>,
}

/// A `Phrase::Define`'s names, each with its checker-closed scheme.
pub type DefineSchemes = Vec<(String, Arc<crate::typecheck::Scheme>)>;

/// One top-level statement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Phrase {
    /// `let p = M` at the top level: run M, extend the session environment.
    /// `schemes` has one entry per name `pattern` binds, closed and
    /// generalised by the checker — a destructuring `let` carries one per
    /// component.
    Define {
        pattern: Arc<IrPattern>,
        comp: Arc<Comp>,
        schemes: DefineSchemes,
    },
    /// Any other statement.
    Run(Arc<Comp>),
}

impl Toplevel {
    /// The scheme each top-level name was closed at, the last `let` of a name
    /// winning as it does in the environment the phrases build.
    pub(crate) fn exported_schemes(&self) -> Vec<(String, Arc<crate::typecheck::Scheme>)> {
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
    pub fn arrow(&self) -> Option<(&IrPattern, &Arc<Self>)> {
        match &self.item {
            CompKind::Lam { param, body } => Some((param, body)),
            CompKind::Rec { group, index } => group.shape()[*index].1.arrow(),
            _ => None,
        }
    }
}

/// True if `top` is a single external/builtin command call.
///
/// A fact about the input's shape, which hosts read to tailor what they say
/// about a failure: exactly one phrase, `Run(c)`, with `c` an `Exec` under
/// its hoisted temporaries and the checker's byte-to-value coercion — the
/// tail `Run`'s value is reported now, so a byte-routed external gets
/// wrapped in `Bind(Capture(_), x, Decode(x))`.
pub(crate) fn is_single_command(top: &Toplevel) -> bool {
    let [phrase] = top.phrases.as_slice() else {
        return false;
    };
    let Phrase::Run(comp) = &phrase.item else {
        return false;
    };
    let mut c = comp.as_ref();
    loop {
        c = match &c.item {
            CompKind::Capture(body) => body,
            CompKind::Bind { comp, rest, .. } if matches!(rest.item, CompKind::Decode(_)) => comp,
            _ => break,
        };
    }
    matches!(c.item, CompKind::Exec(_))
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
            CompKind::Pipeline {
                stages,
                stage_types: _,
            } => {
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
            // No `Register` variant carries a name reference: the five
            // pseudo-variables are computed, and a `~`-path names no variable.
            CompKind::Observe(_) => {}
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

impl Mentions for Args {
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
    Lam { param: Param, body: Arc<Comp> },
    /// return V — produce a value.
    Return(Val),
    /// Build a list, record, or map some of whose parts are spread or keyed
    /// at run time — the one rule that does O(data) work over value syntax.
    Assemble(Assembly),
    /// M to x. N — run `comp`, bind its result, continue with `rest`.  `a; b`
    /// is `a to _. b`, on `Wildcard`.
    Bind {
        comp: Arc<Comp>,
        pattern: Arc<IrPattern>,
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
    Pipeline {
        stages: Vec<Arc<Comp>>,
        /// The inferred value type out of each stage, parallel to `stages`.
        /// Only the structural REPL's typed spine reads it, so an
        /// un-annotated pipeline keeps the `Unit` placeholder harmlessly.
        stage_types: Vec<crate::typecheck::Ty>,
    },
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
    /// A read of the store, in computation position: what `$CWD` and `~/x` are.
    Observe(Register),
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
    /// the value it lets in against.  `None` for every other head.
    pub(crate) site: Option<Arc<crate::types::Site>>,
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

/// A read of the shell's store, in computation position: what `$CWD`,
/// `$ENV`, and a `~`-path are. Never a value — reading the store is an
/// effect, so it names a register rather than being spelled as one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Register {
    Env,
    Args,
    Nproc,
    Cwd,
    User,
    Tilde(TildePath),
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
mod tests {
    use super::*;
    use crate::path::tilde::TildePath;
    use crate::syntax::ast::{BinaryOp, Redirects, StderrTarget, WriteMode};
    use crate::typecheck::Ty;

    #[test]
    fn from_word_classifies_canonical_numbers() {
        assert_eq!(Val::from_word("5"), Val::Int(5));
        assert_eq!(Val::from_word("42"), Val::Int(42));
        assert_eq!(Val::from_word("0"), Val::Int(0));
        assert_eq!(Val::from_word("2.5"), Val::Float(2.5));
        assert_eq!(Val::from_word("true"), Val::Bool(true));
        assert_eq!(Val::from_word("unit"), Val::String("unit".into()));
        assert_eq!(Val::from_word("hello"), Val::String("hello".into()));
    }

    // ── referenced_names: exhaustive walker coverage ─────────────────────

    fn var(name: &str) -> Val {
        Val::Variable(name.into())
    }

    fn svar(name: &str) -> Spanned<Val> {
        Spanned::synthetic(var(name))
    }

    fn ret(name: &str) -> Arc<Comp> {
        Arc::new(Spanned::synthetic(CompKind::Return(var(name))))
    }

    /// One synthetic `Comp` per `CompKind` and `Val` variant: `r_*` labels
    /// what it references, `*_bound` what it merely binds.  The harvest is
    /// asserted *exactly* — a subset would hide a wildcard-arm regression, a
    /// superset a bound name over-renewing.
    #[test]
    fn referenced_names_walks_every_variant() {
        let lam_param = IrPattern::Name("lam_param_bound".into());
        let lam = Spanned::synthetic(CompKind::Lam {
            param: lam_param,
            body: ret("r_lam_body"),
        });

        let bind_pattern = IrPattern::List {
            elems: vec![IrPattern::Name("bind_map_bound".into())],
            rest: Some("bind_rest_bound".into()),
        };
        let bind = Spanned::synthetic(CompKind::Bind {
            comp: ret("r_bind_comp"),
            pattern: Arc::new(bind_pattern),
            rest: ret("r_bind_rest"),
        });

        let app = Spanned::synthetic(CompKind::App {
            head: Arc::new(Spanned::synthetic(CompKind::Force(var("r_app_head")))),
            args: vec![
                ValListElem::Single(svar("r_app_arg_single")),
                ValListElem::Spread(svar("r_app_arg_spread")),
            ],
        });

        let exec_name = Spanned::synthetic(CompKind::Exec(Exec {
            head: CommandWord::Name(CommandName::Bare("r_exec_name_head".into())),
            args: vec![ValListElem::Single(svar("r_exec_arg"))],
            redirects: Redirects {
                stdout: Some((WriteMode::Write, var("r_exec_redirect_target"))),
                ..Redirects::default()
            },
            site: None,
        }));
        let exec_external = Spanned::synthetic(CompKind::Exec(Exec {
            head: CommandWord::External(CommandName::Bare("r_exec_external_head".into())),
            args: vec![],
            // A dup has no operand, so contributes no reference to over-collect.
            redirects: Redirects {
                stderr: Some(StderrTarget::Stdout),
                ..Redirects::default()
            },
            site: None,
        }));

        let pipeline = Spanned::synthetic(CompKind::Pipeline {
            stages: vec![
                Arc::new(Spanned::synthetic(CompKind::Force(var(
                    "r_pipeline_stage1",
                )))),
                Arc::new(Spanned::synthetic(CompKind::Force(var(
                    "r_pipeline_stage2",
                )))),
            ],
            stage_types: vec![Ty::Unit, Ty::Unit],
        });

        let binary = Spanned::synthetic(CompKind::Binary(
            BinaryOp::Add,
            var("r_binary_a"),
            var("r_binary_b"),
        ));
        let not = Spanned::synthetic(CompKind::Not(var("r_not")));
        let index = Spanned::synthetic(CompKind::Index {
            target: var("r_index_target"),
            keys: vec![Spanned::synthetic(var("r_index_key"))],
        });
        let interpolation = Spanned::synthetic(CompKind::Interpolation(vec![
            var("r_interp_a"),
            var("r_interp_b"),
        ]));
        let rec_group: Arc<GroupNode> =
            Node::new(vec![("rec_name_bound".into(), ret("r_rec_member"))].into());
        let rec = Spanned::synthetic(CompKind::Rec {
            group: rec_group,
            index: 0,
        });
        let observe = Spanned::synthetic(CompKind::Observe(Register::Tilde(TildePath {
            user: None,
            suffix: None,
        })));
        let if_ = Spanned::synthetic(CompKind::If {
            cond: Spanned::synthetic(var("r_if_cond")),
            then: svar("r_if_then"),
            else_: svar("r_if_else"),
        });
        let case = Spanned::synthetic(CompKind::Case {
            scrutinee: Spanned::synthetic(var("r_case_scrutinee")),
            arms: vec![CaseArm {
                tag: Spanned::synthetic("some".into()),
                body: svar("r_case_arm_body"),
            }],
        });

        let scope_try = Spanned::synthetic(CompKind::Try {
            body: var("r_try_body"),
            handler: var("r_try_handler"),
        });
        let scope_guard = Spanned::synthetic(CompKind::Guard {
            body: var("r_guard_body"),
            cleanup: var("r_guard_cleanup"),
        });
        let scope_within = Spanned::synthetic(CompKind::Within {
            opts: vec![("dir".into(), svar("r_within_opts"))].into(),
            handlers: Some(vec![HandlerArmV {
                name: "deploy".into(),
                value: Spanned::synthetic(var("r_within_arm")),
            }]),
            body: var("r_within_body"),
        });
        let scope_grant = Spanned::synthetic(CompKind::Grant {
            caps: vec![("net".into(), svar("r_grant_caps"))].into(),
            body: var("r_grant_body"),
        });
        let scope_audit = Spanned::synthetic(CompKind::Audit {
            body: var("r_audit_body"),
        });
        let scope_redirect = Spanned::synthetic(CompKind::Redirect {
            body: ret("r_scope_redirect_body"),
            redirects: Redirects {
                stderr: Some(StderrTarget::File(
                    WriteMode::Append,
                    var("r_scope_redirect_target"),
                )),
                ..Redirects::default()
            },
        });
        let val_list = Spanned::synthetic(CompKind::Return(Val::list(vec![
            Spanned::synthetic(Val::Unit),
            Spanned::synthetic(Val::String("s".into())),
            Spanned::synthetic(Val::Int(1)),
            Spanned::synthetic(Val::Float(1.0)),
            Spanned::synthetic(Val::Bool(true)),
            svar("r_list_single"),
        ])));
        let val_record = Spanned::synthetic(CompKind::Return(Val::record(vec![(
            "lbl".into(),
            svar("r_record_value"),
        )])));
        let val_map = Spanned::synthetic(CompKind::Return(Val::map(vec![(
            "lbl".into(),
            svar("r_map_value"),
        )])));
        let assemble_list = Spanned::synthetic(CompKind::Assemble(Assembly::List(vec![
            ValListElem::Single(svar("r_assemble_list_single")),
            ValListElem::Spread(svar("r_assemble_list_spread")),
        ])));
        let assemble_record = Spanned::synthetic(CompKind::Assemble(Assembly::Record(vec![
            ValRecordEntry::Field("lbl".into(), svar("r_assemble_record_value")),
            ValRecordEntry::Spread(svar("r_assemble_record_spread")),
        ])));
        let assemble_map = Spanned::synthetic(CompKind::Assemble(Assembly::Map(vec![
            ValMapEntry::Entry(var("r_assemble_map_key"), svar("r_assemble_map_value")),
            ValMapEntry::Spread(svar("r_assemble_map_spread")),
        ])));
        let val_variant = Spanned::synthetic(CompKind::Return(Val::Variant {
            label: "lbl".into(),
            payload: Some(Box::new(var("r_variant_payload"))),
        }));
        let val_variant_empty = Spanned::synthetic(CompKind::Return(Val::Variant {
            label: "lbl_empty".into(),
            payload: None,
        }));
        let val_thunk = Spanned::synthetic(CompKind::Return(Val::thunk(Arc::new(
            Spanned::synthetic(CompKind::Return(var("r_thunk_body"))),
        ))));
        let capture = Spanned::synthetic(CompKind::Capture(ret("r_capture_body")));
        let decode = Spanned::synthetic(CompKind::Decode(var("r_decode_body")));

        // No single `CompKind` holds an arbitrary list of sub-`Comp`s to
        // wrap all of the above in one tree, so each is walked on its own
        // and the harvests are unioned.
        let nodes: Vec<Arc<Comp>> = vec![
            Arc::new(Spanned::synthetic(CompKind::Force(var("r_force")))),
            Arc::new(Spanned::synthetic(CompKind::Return(var("r_return")))),
            Arc::new(lam),
            Arc::new(bind),
            Arc::new(app),
            Arc::new(exec_name),
            Arc::new(exec_external),
            Arc::new(pipeline),
            Arc::new(binary),
            Arc::new(not),
            Arc::new(index),
            Arc::new(interpolation),
            Arc::new(rec),
            Arc::new(observe),
            Arc::new(if_),
            Arc::new(case),
            Arc::new(scope_try),
            Arc::new(scope_guard),
            Arc::new(scope_within),
            Arc::new(scope_grant),
            Arc::new(scope_audit),
            Arc::new(scope_redirect),
            Arc::new(val_list),
            Arc::new(val_record),
            Arc::new(val_map),
            Arc::new(assemble_list),
            Arc::new(assemble_record),
            Arc::new(assemble_map),
            Arc::new(val_variant),
            Arc::new(val_variant_empty),
            Arc::new(val_thunk),
            Arc::new(capture),
            Arc::new(decode),
        ];

        let found: std::collections::HashSet<&str> = nodes
            .iter()
            .flat_map(|c| {
                let mut out = Vec::new();
                c.mentions(&mut out);
                out.into_iter().map(AsRef::as_ref).collect::<Vec<_>>()
            })
            .collect();

        let expected = [
            "r_force",
            "r_return",
            "r_lam_body",
            "r_bind_comp",
            "r_bind_rest",
            "r_app_head",
            "r_app_arg_single",
            "r_app_arg_spread",
            "r_exec_name_head",
            "r_exec_arg",
            "r_exec_redirect_target",
            "r_exec_external_head",
            "r_pipeline_stage1",
            "r_pipeline_stage2",
            "r_binary_a",
            "r_binary_b",
            "r_not",
            "r_index_target",
            "r_index_key",
            "r_interp_a",
            "r_interp_b",
            "r_rec_member",
            "r_if_cond",
            "r_if_then",
            "r_if_else",
            "r_case_scrutinee",
            "r_case_arm_body",
            "r_try_body",
            "r_try_handler",
            "r_guard_body",
            "r_guard_cleanup",
            "r_within_opts",
            "r_within_arm",
            "r_within_body",
            "r_grant_caps",
            "r_grant_body",
            "r_audit_body",
            "r_scope_redirect_body",
            "r_scope_redirect_target",
            "r_list_single",
            "r_record_value",
            "r_map_value",
            "r_assemble_list_single",
            "r_assemble_list_spread",
            "r_assemble_record_value",
            "r_assemble_record_spread",
            "r_assemble_map_key",
            "r_assemble_map_value",
            "r_assemble_map_spread",
            "r_variant_payload",
            "r_thunk_body",
            "r_capture_body",
            "r_decode_body",
        ];

        for name in expected {
            assert!(found.contains(name), "missing reference: {name}");
        }
        assert_eq!(
            found.len(),
            expected.len(),
            "unexpected extra name in {found:?}; every bound-not-referenced \
             name (lam_param_bound, bind_map_bound, bind_rest_bound, \
             rec_name_bound) must be absent"
        );

        for bound in [
            "lam_param_bound",
            "bind_map_bound",
            "bind_rest_bound",
            "rec_name_bound",
        ] {
            assert!(
                !found.contains(bound),
                "a bound (not referenced) name leaked into the harvest: {bound}"
            );
        }
    }

    /// A thunk mentioning `b`, `a`, `b` and nesting a thunk over `c`, `a` has
    /// occ `[a, b, c]`: sorted, distinct, and read off the inner node rather
    /// than by re-walking its body.
    #[test]
    fn occ_is_sorted_distinct_and_nested_nodes_are_not_walked() {
        let inner = Val::thunk(Arc::new(Spanned::synthetic(CompKind::Binary(
            BinaryOp::Add,
            var("c"),
            var("a"),
        ))));
        let outer = ThunkNode::new(Arc::new(Spanned::synthetic(CompKind::Interpolation(vec![
            var("b"),
            var("a"),
            var("b"),
            inner,
        ]))));
        let occ = outer.occ();
        assert_eq!(occ.len(), 3, "sorted and distinct: a, b, c");
        for name in ["a", "b", "c"] {
            assert!(occ.contains(name), "occ missing {name}");
        }
    }

    /// A list node serialises to its shape alone, and decodes with the same
    /// occ it was built with.
    #[test]
    fn a_node_crosses_the_wire_as_its_shape() {
        let node = ListNode::new(vec![svar("a"), svar("a"), svar("b")].into());
        let wire = serde_json::to_string(&node).expect("serialize node");
        let shape_wire = serde_json::to_string(node.shape()).expect("serialize shape");
        assert_eq!(wire, shape_wire, "the wire carries the shape alone");

        let decoded: Arc<ListNode> = serde_json::from_str(&wire).expect("deserialize node");
        assert_eq!(decoded.occ(), node.occ());
    }
}
