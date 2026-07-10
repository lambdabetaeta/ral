//! Call-by-push-value intermediate representation.
//!
//! The IR is the target of elaboration ([`crate::elaborator`]) and the
//! input to evaluation.  It follows a *call-by-push-value* (CBPV)
//! discipline: [`Val`] is inert data (strings, lists, maps, thunks),
//! [`Comp`] is effectful and sequenced.  This split guarantees that
//! effects are always explicit — a value can never diverge or perform I/O.
//!
//! Every [`Comp`] node carries an optional [`crate::source::Span`] for error reporting
//! (synthetic nodes — builtins, prelude, generated code — have `span: None`).
//! Sub-`Val` positions inside a `Comp` that the typechecker narrows onto
//! (`If.cond`, `Case.scrutinee`/`table`, per-arg in `Args`, per-key in
//! `Index.keys`) use the [`Spanned`] wrapper from the AST, so the span
//! rides with the value rather than being parked on the parent.

use crate::mode::{ByteMode, Wire};
use crate::path::tilde::TildePath;
use crate::source::Spanned;
use crate::syntax::ast::{BinaryOp, Pattern, RedirectMode};

/// IR-side pattern.  Identical in shape to [`crate::syntax::ast::Pattern`] but with
/// map-pattern defaults represented as pre-elaborated computations
/// ([`Arc<Comp>`]) instead of raw [`crate::syntax::ast::Ast`].  This is what the
/// elaborator hands to the typechecker and the evaluator — no parser
/// syntax survives the elaboration phase.
pub type IrPattern = Pattern<Arc<Comp>>;
/// IR-side lambda parameter — a pattern with elaborated defaults.
pub type Param = IrPattern;

// ── Values ──────────────────────────────────────────────────────────────
//
// `Val` is CBPV's value category: inert data requiring no evaluation.
// Typed numeric and boolean literals (`Int`, `Float`, `Bool`) exist so
// that `$[...]` can lower into plain `Bind`-sequences without going
// through string-literal parsing.
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Structured command head for external dispatch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CommandName {
    /// Ordinary unresolved head, subject to alias/builtin/PATH lookup.
    Bare(String),
    /// Slash-bearing literal path, executed directly.
    Path(String),
    /// Tilde-prefixed path, expanded only at the process boundary.
    TildePath(TildePath),
}

impl CommandName {
    pub fn bare(&self) -> Option<&str> {
        match self {
            Self::Bare(name) => Some(name),
            Self::Path(_) | Self::TildePath(_) => None,
        }
    }
}

/// A value — inert data, no effects.
///
/// Values are the CBPV value category: they require no evaluation and
/// can be passed, stored, and pattern-matched freely.  The evaluator
/// produces values; computations consume and return them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Val {
    /// The unit value — result of side-effect-only computations.
    Unit,
    /// A string value.  Produced by quoted source (`'x'` / `"x"`) and
    /// by bare-word elaboration after [`Val::from_word`] has decided
    /// the token isn't `true`/`false`/`unit` or numeric — see the
    /// elaborator for the single classification point.
    String(String),
    /// Integer literal.  Produced by `$[…]` expressions and by
    /// [`Val::from_word`] on bare-word tokens that parse as `i64`.
    Int(i64),
    /// Floating-point literal.  Produced by `$[…]` expressions and by
    /// [`Val::from_word`] on bare-word tokens that contain `.` and
    /// parse as `f64`.
    Float(f64),
    /// Boolean literal.  Produced by `$[…]` expressions and by
    /// [`Val::from_word`] on the bare words `true` and `false`.
    Bool(bool),
    /// A bound variable reference, resolved at evaluation time.
    Variable(String),
    /// A suspended computation (CBPV thunk).  Created by `{ … }` blocks
    /// and lambda abstractions; eliminated by `Force`.
    Thunk(Arc<Comp>),
    /// A list literal, possibly containing spread (`...x`) elements.
    List(Vec<ValListElem>),
    /// A map literal, possibly containing spread (`...x`) entries.
    Map(Vec<ValMapEntry>),
    /// A variant constructor: `` `label `` (no payload) or `` `label payload ``.
    /// The label is stored without its leading backtick.
    Variant {
        label: String,
        payload: Option<Box<Self>>,
    },
    /// Home-directory expansion: `~`, `~user`, `~/path`, or `~user/path`.
    TildePath(TildePath),
}

impl Val {
    /// Classify a bare-word token (`Ast::Word::Plain` / `Ast::Word::Slash`)
    /// into the most specific [`Val`] variant, falling back to
    /// [`Val::String`] when the word doesn't match a literal shape.
    /// The shape rules live in [`crate::syntax::ast::WordLiteral::classify`].
    ///
    /// This is a known defect: classification is eager and type-blind, so
    /// a numeric-looking bare word meant as argv data is read as a number
    /// and stringified back losslessly only when its canonical form matches
    /// its source (`007` ⇒ `7`, `1.50` ⇒ `1.5`). The planned fix moves
    /// classification into type-directed inference; see
    /// `dev/docs/260611_overloaded-literals.md`.
    pub fn from_word(s: &str) -> Self {
        use crate::syntax::ast::WordLiteral;
        match WordLiteral::classify(s) {
            Some(WordLiteral::Bool(b)) => Self::Bool(b),
            Some(WordLiteral::Unit) => Self::Unit,
            Some(WordLiteral::Int(n)) => Self::Int(n),
            Some(WordLiteral::Float(f)) => Self::Float(f),
            None => Self::String(s.to_string()),
        }
    }
}

/// An element of a list literal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ValListElem {
    /// A single element.
    Single(Val),
    /// A spread element (`...x`), spliced into the surrounding list.
    Spread(Val),
}

/// An entry of a map literal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ValMapEntry {
    /// A key-value pair.
    Entry(Val, Val),
    /// A spread entry (`...x`), merged into the surrounding map.
    Spread(Val),
}

/// Positional arguments to a call (`App` or `Exec`).
///
/// Each element is a [`Spanned<ValListElem>`] — the [`Spanned`] carries
/// the parser's byte range for the whole argument slot (including the
/// `...` for spread), so per-argument diagnostics underline the offending
/// arg.  Synthetic / hoisted calls use [`Spanned::synthetic`] entries.
///
/// Conceptually distinct from a list *value* (`Val::List`): args have
/// positional semantics, spread splices into the call's argument list,
/// and the typechecker narrows source positions per-element.  The shape
/// is the same — `Single`/`Spread` of `Val` — but giving the concept its
/// own type lets the spans live with the values rather than parked on
/// the enclosing `Comp`.
pub type Args = Vec<Spanned<ValListElem>>;

/// Helpers that interpret an [`Args`] as positional arguments.  Free
/// functions rather than methods because [`Args`] is a type alias.
pub mod args {
    use super::{Args, Val, ValListElem};

    /// Walk every sub-value in the args list, regardless of whether each
    /// element is `Single` or `Spread`.  Used by passes that need to
    /// visit every Val in argument position without distinguishing the
    /// two (mode analysis, best-effort sub-expression typing).
    pub fn iter_subvals(args: &Args) -> impl Iterator<Item = &Val> {
        args.iter().map(|e| match &e.item {
            ValListElem::Single(v) | ValListElem::Spread(v) => v,
        })
    }

    /// View the args as a literal positional-arg list, if statically
    /// possible: no `Spread` elements.  Returns `None` for spread-bearing
    /// calls — those have dynamic arity and demand weaker static checks.
    pub fn positional(args: &Args) -> Option<Vec<&Val>> {
        let mut out = Vec::with_capacity(args.len());
        for e in args {
            match &e.item {
                ValListElem::Single(v) => out.push(v),
                ValListElem::Spread(_) => return None,
            }
        }
        Some(out)
    }
}

/// Target of an I/O redirect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ValRedirectTarget {
    /// Redirect to/from a file path.
    File(Val),
    /// Redirect to/from a file descriptor number.
    Fd(u32),
}

/// IR-side I/O redirect: file descriptor, mode, and target value.
/// Owned as a field of [`CompKind::Exec`] (fused into the spawn syscall,
/// which installs descriptors and execs atomically) or
/// [`ScopeOp::Redirect`] (a redirect-frame scope wrapping an arbitrary
/// body).  Never appears as a wrapper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RedirectV {
    pub fd: u32,
    pub mode: RedirectMode,
    pub target: ValRedirectTarget,
}

// ── Computations ────────────────────────────────────────────────────────

/// A computation node — effectful, sequenced — with an optional source span.
///
/// This is the primary IR type that the evaluator interprets.  Every node
/// carries its own [`crate::source::Span`] (via the wrapping `Spanned`), set once during
/// elaboration, so error messages can point back to the originating source
/// text.  Synthetic nodes (builtins, prelude, generated code) carry
/// `span: None`.
pub type Comp = Spanned<CompKind>;

/// True if this computation is a single external/builtin command call.
/// Used to suppress the ariadne source-span arrow when the entire
/// input is just one command.
pub fn is_single_command(comp: &Comp) -> bool {
    match &comp.item {
        CompKind::Exec(_) => true,
        CompKind::Seq(stmts) => {
            let mut commands = stmts.iter();
            matches!(commands.next().map(|c| &c.item), Some(CompKind::Exec(_)))
                && commands.next().is_none()
        }
        _ => false,
    }
}

/// Every name `comp` can reference: [`Val::Variable`] occurrences and
/// `Exec`/`^` command-head names, collected by an exhaustive walk over
/// `CompKind` / `Val` / `IrPattern` map-defaults / redirect targets
/// (`decisions/260629_agent-binding-reaping`, the binding-lease ledger's
/// renewal harvest). No wildcard arm anywhere in the walk: a new `CompKind`
/// or `Val` variant is a compile error here rather than a silently
/// unharvested reference. Over-approximate by design — a name in an
/// untaken branch (the other arm of an `if`, an unmatched `case` table
/// entry) still renews, which only ever lengthens a lease, never shortens
/// one.
pub(crate) fn referenced_names(comp: &Comp) -> Vec<&str> {
    let mut out = Vec::new();
    walk_comp(comp, &mut out);
    out
}

fn walk_comp<'a>(comp: &'a Comp, out: &mut Vec<&'a str>) {
    match &comp.item {
        CompKind::Lam { param, body } => {
            walk_pattern_defaults(param, out);
            walk_comp(body, out);
        }
        CompKind::Bind {
            comp,
            pattern,
            rest,
            scheme: _,
            rhs_output: _,
        } => {
            walk_comp(comp, out);
            walk_pattern_defaults(pattern, out);
            walk_comp(rest, out);
        }
        CompKind::App { head, args } => {
            walk_comp(head, out);
            walk_args(args, out);
        }
        CompKind::Exec(exec) => {
            walk_command_word(&exec.head, out);
            walk_args(&exec.args, out);
            walk_redirects(&exec.redirects, out);
        }
        CompKind::Pipeline {
            stages,
            wires: _,
            stage_types: _,
        } => {
            for stage in stages {
                walk_comp(stage, out);
            }
        }
        CompKind::Binary(_op, a, b) => {
            walk_val(a, out);
            walk_val(b, out);
        }
        CompKind::Force(v) | CompKind::Return(v) | CompKind::Not(v) => walk_val(v, out),
        CompKind::Index { target, keys } => {
            walk_val(target, out);
            for key in keys {
                walk_val(&key.item, out);
            }
        }
        CompKind::Chain(comps) | CompKind::Seq(comps) => {
            for c in comps {
                walk_comp(c, out);
            }
        }
        CompKind::Interpolation(vals) => {
            for v in vals {
                walk_val(v, out);
            }
        }
        CompKind::LetRec { slot: _, bindings } => {
            for (_name, rhs) in bindings.iter() {
                walk_val(rhs, out);
            }
        }
        CompKind::If { cond, then, else_ } => {
            walk_val(&cond.item, out);
            walk_comp(then, out);
            walk_comp(else_, out);
        }
        CompKind::Case { scrutinee, table } => {
            walk_val(&scrutinee.item, out);
            walk_val(&table.item, out);
        }
        CompKind::Scope(op) => walk_scope_op(op, out),
    }
}

fn walk_val<'a>(val: &'a Val, out: &mut Vec<&'a str>) {
    match val {
        Val::Unit
        | Val::String(_)
        | Val::Int(_)
        | Val::Float(_)
        | Val::Bool(_)
        | Val::TildePath(_) => {}
        Val::Variable(name) => out.push(name),
        Val::Thunk(comp) => walk_comp(comp, out),
        Val::List(elems) => {
            for elem in elems {
                match elem {
                    ValListElem::Single(v) | ValListElem::Spread(v) => walk_val(v, out),
                }
            }
        }
        Val::Map(entries) => {
            for entry in entries {
                match entry {
                    ValMapEntry::Entry(k, v) => {
                        walk_val(k, out);
                        walk_val(v, out);
                    }
                    ValMapEntry::Spread(v) => walk_val(v, out),
                }
            }
        }
        Val::Variant { label: _, payload } => {
            if let Some(p) = payload {
                walk_val(p, out);
            }
        }
    }
}

fn walk_args<'a>(args: &'a Args, out: &mut Vec<&'a str>) {
    for spanned in args {
        match &spanned.item {
            ValListElem::Single(v) | ValListElem::Spread(v) => walk_val(v, out),
        }
    }
}

/// The head name of both dispatch forms — `Name` (binding → handler → PATH)
/// and `^name` (handler → PATH, skipping binding) — collected regardless of
/// dispatch shape: over-approximating a name a `^`-bypassed head could never
/// actually renew from is harmless, the same safe direction as an untaken
/// branch.
fn walk_command_word<'a>(word: &'a CommandWord, out: &mut Vec<&'a str>) {
    if let CommandName::Bare(name) = word.name() {
        out.push(name);
    }
}

fn walk_redirects<'a>(redirects: &'a [RedirectV], out: &mut Vec<&'a str>) {
    for redirect in redirects {
        match &redirect.target {
            ValRedirectTarget::File(v) => walk_val(v, out),
            ValRedirectTarget::Fd(_) => {}
        }
    }
}

/// Map-pattern defaults are the only sub-position of an [`IrPattern`] that
/// can reference a name — the pattern's own names are bound, not
/// referenced. Recurses through `List` elements so a nested destructuring
/// pattern's defaults are found too.
fn walk_pattern_defaults<'a>(pattern: &'a IrPattern, out: &mut Vec<&'a str>) {
    match pattern {
        IrPattern::Wildcard | IrPattern::Name(_) => {}
        IrPattern::List { elems, rest: _ } => {
            for elem in elems {
                walk_pattern_defaults(elem, out);
            }
        }
        IrPattern::Map(entries) => {
            for entry in entries {
                walk_pattern_defaults(&entry.pattern, out);
                if let Some(default) = &entry.default {
                    walk_comp(default, out);
                }
            }
        }
    }
}

fn walk_scope_op<'a>(op: &'a ScopeOp, out: &mut Vec<&'a str>) {
    match op {
        ScopeOp::Try { body, handler } => {
            walk_val(body, out);
            walk_val(handler, out);
        }
        ScopeOp::Guard { body, cleanup } => {
            walk_val(body, out);
            walk_val(cleanup, out);
        }
        ScopeOp::Within { opts, body } => {
            walk_val(opts, out);
            walk_val(body, out);
        }
        ScopeOp::Grant { caps, body } => {
            walk_val(caps, out);
            walk_val(body, out);
        }
        ScopeOp::Audit { body } => walk_val(body, out),
        ScopeOp::Redirect { body, redirects } => {
            walk_comp(body, out);
            walk_redirects(redirects, out);
        }
    }
}

/// The computation proper — the CBPV computation category.
///
/// Each variant corresponds to a distinct form of effectful term.
/// The evaluator pattern-matches on `CompKind` to step the computation.
/// Notation in variant docs follows Levy's CBPV conventions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CompKind {
    /// force V — run a thunk (CBPV force).
    Force(Val),
    /// A lambda abstraction — a computation that, when evaluated, captures a closure.
    Lam { param: Param, body: Arc<Comp> },
    /// return V — produce a value.
    Return(Val),
    /// M to x. N — sequence: run M, bind result to x, continue with N.
    Bind {
        comp: Arc<Comp>,
        pattern: IrPattern,
        rest: Arc<Comp>,
        /// The checker's generalised scheme for the bound name, written
        /// by the annotation pass for a top-level `Bind` with a `Name`
        /// pattern; the evaluator installs it next to the value so the
        /// next turn's check can be seeded from the live binding.  Closed
        /// (every variable ground or quantified) so it survives across
        /// per-turn unifiers.  `None` until the checker runs, and always
        /// `None` for destructuring patterns.  Boxed so the optional
        /// scheme stays one pointer wide on the `Bind` node.
        scheme: Option<Box<crate::typecheck::Scheme>>,
        /// The ground output mode for the bound computation `comp`: the
        /// evaluator's bind rule reads it to decide stdout capture.  The
        /// elaborator emits the [`ByteMode::Empty`] placeholder; the
        /// annotation pass overwrites it with the checker's verdict.
        rhs_output: ByteMode,
    },
    /// CBPV application: head is a function-typed computation, `args` is
    /// the positional argument list.  Typing: `M : A → B, V : A ⊢ M V : B`.
    ///
    /// This is the lambda-calculus elimination form, used when the head
    /// resolves to a bound value (`$f x`, `(|x| body) x`, etc.).  It does
    /// not carry redirects: redirects are a shell effect, not a property
    /// of function application.  Trailing redirects around a CBPV
    /// application elaborate to [`ScopeOp::Redirect`] wrapping a thunk of
    /// the `App`.
    ///
    /// Each argument is a [`Spanned<ValListElem>`] — `Single`/`Spread` of
    /// [`Val`] paired with the parser's byte range for that argument slot,
    /// so the typechecker can narrow a per-argument unification failure
    /// onto the offending argument rather than the whole call.  A
    /// list-literal argument (`f [...$xs]`) is one `Single`-wrapped
    /// `Val::List`; an argument-position spread (`f ...$xs`) is a
    /// `Spread`.  Synthetic / hoisted applications use
    /// [`Spanned::synthetic`] entries.
    App { head: Arc<Comp>, args: Args },
    /// Shell command invocation: name-dispatched (binding → handler →
    /// PATH) or pinned to a statically-resolved builtin.  Trailing I/O
    /// redirects are fused into the call itself: the spawn syscall installs
    /// descriptors and execs atomically, so the redirect list is a field of
    /// `Exec` rather than a wrapping scope.
    ///
    /// This is the effect-boundary form: anything outside this variant
    /// cannot reach the handler/PATH dispatch chain or an external
    /// program.  CBPV applications go through [`CompKind::App`].
    Exec(Exec),
    /// Pipeline: concurrent stages connected by Unix pipes.
    /// Each stage runs in parallel; stdout of stage N feeds stdin of stage N+1.
    Pipeline {
        stages: Vec<Arc<Comp>>,
        /// One ground [`Wire`] per stage — exactly `stages.len()` of them.
        /// The elaborator emits an all-[`Wire::EMPTY`] placeholder; the
        /// checker's annotation pass overwrites it with the inferred byte
        /// channels, which pipeline staging then reads to wire each stage.
        /// A `Wire` is `Copy` and rides unboxed.
        wires: Vec<Wire>,
        /// One inferred *value* type per stage — the data flowing out of
        /// it — parallel to `stages`/`wires`.  The elaborator emits a
        /// `Unit` placeholder; the annotation pass overwrites it with the
        /// resolved per-stage types.  Retained for the structural REPL's
        /// typed spine; the evaluator never reads it, so an un-annotated
        /// pipeline (which never reaches the evaluator) keeps the
        /// placeholder harmlessly.
        stage_types: Vec<crate::typecheck::Ty>,
    },
    /// Binary primitive on already-evaluated values (`$[a + b]`,
    /// `$[a == b]`, …).  Arity-correct by construction — the inner
    /// `BinaryOp` excludes the unary `not`, which has its own
    /// [`CompKind::Not`] variant.
    Binary(BinaryOp, Val, Val),
    /// Unary logical negation: `not v` on a `Bool` value.  Separate
    /// from [`CompKind::Binary`] so the IR can't represent the
    /// nonsense "`Not` applied to two operands" / "`Add` applied to
    /// one operand"; the evaluator and typechecker dispatch on
    /// variant rather than a runtime arity guard.
    Not(Val),
    /// Indexing: `V[k1][k2]` — eliminate a collection value by a sequence
    /// of key values.  Computation-typed only because it can fail
    /// (key not found, out of bounds); target and keys are pure values.
    ///
    /// Each key is a [`Spanned<Val>`] carrying the byte range the
    /// parser read for that `[k]` (including the surrounding brackets),
    /// so a per-key unification failure underlines the offending key.
    /// Synthetic fixtures use [`Spanned::synthetic`].
    Index {
        target: Val,
        keys: Vec<Spanned<Val>>,
    },
    /// Fallback chain (`a ? b ? c`): try each computation in order;
    /// return the first that succeeds.
    Chain(Vec<Arc<Comp>>),
    /// String interpolation (effectful — variable lookups can fail).
    Interpolation(Vec<Val>),
    /// Sequence of computations (last value is the result).
    Seq(Vec<Arc<Comp>>),
    /// Simultaneous fixed point for mutually recursive functions.  Each
    /// binding's RHS is a thunk value (CBPV: mutual recursion requires the
    /// fixpoint to close over thunked references to its siblings).
    /// slot = None: establish all bindings in the current shell, return Unit.
    /// slot = Some(i): re-establish group in a temporary scope, return lambda for binding i.
    LetRec {
        slot: Option<usize>,
        bindings: Arc<Vec<(String, Val)>>,
    },
    /// Conditional: `cond` is a Bool value; the chosen branch (`then`
    /// or `else_`) runs inline.  CBPV: `if V then M else N` with
    /// `V : Bool` and `M, N : C` for the same comp type `C`.
    ///
    /// `cond` is a [`Spanned<Val>`]; its span is the byte range the
    /// parser captured for the condition expression, used by the
    /// typechecker to underline the offending cond on a non-Bool
    /// diagnostic.  Synthesised conditionals (short-circuit `&&` /
    /// `||` lowering, nested elsif arms) use [`Spanned::synthetic`] —
    /// the typechecker falls back to the enclosing pos.
    If {
        cond: Spanned<Val>,
        then: Arc<Comp>,
        else_: Arc<Comp>,
    },
    /// Sum eliminator: `scrutinee` is a variant value; `table` is a
    /// tag-keyed record of thunks; the matching handler is forced on
    /// the payload.  Typechecking guarantees coverage; an unmatched
    /// label at runtime is an internal error.
    ///
    /// `scrutinee` and `table` are each [`Spanned<Val>`] carrying
    /// the parser ranges of the surface operands so the typechecker
    /// can narrow a "case needs a variant" diagnostic onto the
    /// scrutinee, and any handler-shape diagnostic onto the table.
    /// Synthetic IR uses [`Spanned::synthetic`].
    Case {
        scrutinee: Spanned<Val>,
        table: Spanned<Val>,
    },
    /// Effect-frame scope: install an effect for the duration of a
    /// body, then restore.  Includes the control-operator forms
    /// (`try`/`guard`/`within`/`grant`/`audit`) and `redirect`, which
    /// is the redirect-frame scope used whenever a non-`Exec` body
    /// needs trailing I/O redirects.
    Scope(ScopeOp),
}

/// Body of a [`CompKind::Exec`].  Carries its own redirect list, fused
/// into the syscall — there is no separate redirect wrapper for shell
/// calls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exec {
    pub head: CommandWord,
    /// Positional arguments, mirroring [`CompKind::App`]'s `args`.
    /// Each element coerces to its string form at the syscall
    /// boundary; spread (`^cmd ...$xs`) is a `Spread` element.
    pub args: Args,
    pub redirects: Vec<RedirectV>,
}

/// Dispatch shape of an [`Exec`] head.  Two variants, two rules:
/// `Name` goes through binding, handler, then PATH lookup;
/// `External` is the `^name` form, which skips binding but is still
/// contained by any enclosing
/// `within [handlers:]` frame.  The `^name` form is its own variant
/// rather than a flag on `Name` so the IR shape carries the dispatch
/// decision instead of burying it as a boolean.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CommandWord {
    /// Name-dispatched call: binding → handler → PATH at evaluation
    /// time.
    Name(CommandName),
    /// `^name` form: skip binding lookup and resolve through user
    /// handlers, then PATH. Still subject to `within [handlers:]`
    /// containment — the bypass is on the binding lookup, not on the frame.
    External(CommandName),
}

impl CommandWord {
    /// The head name, common to both variants.  Consumers that need
    /// to distinguish `Name` from `External` (the `^name` form) match
    /// on the variant directly; the dispatch decision lives in the
    /// tag, not in a boolean projection.
    pub fn name(&self) -> &CommandName {
        match self {
            Self::Name(n) | Self::External(n) => n,
        }
    }
}

/// The effect-frame variants.  Each describes a particular effect
/// installed for the duration of a body computation, then restored
/// when the body returns or escapes.  Carried directly by
/// [`CompKind::Scope`] — there is no wrapper struct, since no
/// invariant lives outside the variant payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ScopeOp {
    /// `try BODY HANDLER` — run `body`, catching any error and
    /// dispatching to `handler` (a thunk of one argument).
    Try { body: Val, handler: Val },
    /// `guard BODY CLEANUP` — run `body`, then unconditionally run
    /// `cleanup` (a thunk).  Cleanup failures are reported but do not
    /// mask the body's result.
    Guard { body: Val, cleanup: Val },
    /// `within OPTS BODY` — install option overrides for the duration
    /// of `body`.  `opts` is the option map (evaluated at runtime);
    /// `body` is a thunk-shaped value invoked under the scope.
    Within { opts: Val, body: Val },
    /// `grant CAPS BODY` — attenuate the active capability set across
    /// `body`.  `caps` describes the capability map; `body` is a
    /// thunk-shaped value invoked under the attenuated frame.
    Grant { caps: Val, body: Val },
    /// `audit BODY` — run `body` while recording an audit subtree;
    /// the resulting node is reified into a value.
    Audit { body: Val },
    /// Redirect frame: install the given redirects, evaluate `body`,
    /// restore on exit.  Used whenever a CBPV `App` or a nested
    /// `Scope` carries trailing I/O redirects; `Exec` fuses its
    /// redirects directly and does not go through this variant.
    /// `body` is an `Arc<Comp>` rather than a thunk-shaped `Val` —
    /// the redirect frame always wraps a computation, so the IR
    /// makes that statically true and the invoke arm needs no
    /// runtime fallback.
    Redirect {
        body: Arc<Comp>,
        redirects: Vec<RedirectV>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::Wire;
    use crate::path::tilde::TildePath;
    use crate::syntax::ast::{BinaryOp, RedirectMode};
    use crate::typecheck::Ty;

    /// A numeric-looking bare word classifies to its `Int`/`Float`
    /// reading; `true`/`unit` to `Bool`/`Unit`; anything else to `String`.
    #[test]
    fn from_word_classifies_canonical_numbers() {
        assert_eq!(Val::from_word("5"), Val::Int(5));
        assert_eq!(Val::from_word("42"), Val::Int(42));
        assert_eq!(Val::from_word("0"), Val::Int(0));
        assert_eq!(Val::from_word("2.5"), Val::Float(2.5));
        assert_eq!(Val::from_word("true"), Val::Bool(true));
        assert_eq!(Val::from_word("unit"), Val::Unit);
        assert_eq!(Val::from_word("hello"), Val::String("hello".into()));
    }

    // ── referenced_names: exhaustive walker coverage ─────────────────────

    fn var(name: &str) -> Val {
        Val::Variable(name.to_string())
    }

    fn ret(name: &str) -> Arc<Comp> {
        Arc::new(Spanned::synthetic(CompKind::Return(var(name))))
    }

    /// One synthetic `Comp` exercising every `CompKind`, every `Val`, and
    /// every `ScopeOp` variant, each tagging the names it references with a
    /// distinct `r_*` label and the names it merely *binds* (pattern names,
    /// a `LetRec` group's own names) with a `bound_*`/`*_bound` label that
    /// must never appear in the harvest. Asserts the walker finds exactly
    /// the referenced set — not a subset (a wildcard-arm regression would
    /// silently drop one), not a superset (a bound name leaking in would
    /// over-renew in a way this test, not just the type system, must catch).
    #[test]
    fn referenced_names_walks_every_variant() {
        let lam_param = IrPattern::Map(vec![crate::syntax::ast::MapPatternEntry {
            key: crate::syntax::ast::MapKey::Bare("p".into()),
            pattern: IrPattern::Name("lam_param_bound".into()),
            default: Some(Arc::new(Spanned::synthetic(CompKind::Return(var(
                "r_lam_default",
            ))))),
        }]);
        let lam = Spanned::synthetic(CompKind::Lam {
            param: lam_param,
            body: ret("r_lam_body"),
        });

        let bind_pattern = IrPattern::List {
            elems: vec![IrPattern::Map(vec![crate::syntax::ast::MapPatternEntry {
                key: crate::syntax::ast::MapKey::Bare("k".into()),
                pattern: IrPattern::Name("bind_map_bound".into()),
                default: Some(Arc::new(Spanned::synthetic(CompKind::Return(var(
                    "r_bind_pattern_default",
                ))))),
            }])],
            rest: Some("bind_rest_bound".into()),
        };
        let bind = Spanned::synthetic(CompKind::Bind {
            comp: ret("r_bind_comp"),
            pattern: bind_pattern,
            rest: ret("r_bind_rest"),
            scheme: None,
            rhs_output: crate::mode::ByteMode::Empty,
        });

        let app = Spanned::synthetic(CompKind::App {
            head: Arc::new(Spanned::synthetic(CompKind::Force(var("r_app_head")))),
            args: vec![
                Spanned::synthetic(ValListElem::Single(var("r_app_arg_single"))),
                Spanned::synthetic(ValListElem::Spread(var("r_app_arg_spread"))),
            ],
        });

        let exec_name = Spanned::synthetic(CompKind::Exec(Exec {
            head: CommandWord::Name(CommandName::Bare("r_exec_name_head".into())),
            args: vec![Spanned::synthetic(ValListElem::Single(var("r_exec_arg")))],
            redirects: vec![RedirectV {
                fd: 1,
                mode: RedirectMode::Write,
                target: ValRedirectTarget::File(var("r_exec_redirect_target")),
            }],
        }));
        let exec_external = Spanned::synthetic(CompKind::Exec(Exec {
            head: CommandWord::External(CommandName::Bare("r_exec_external_head".into())),
            args: vec![],
            redirects: vec![RedirectV {
                fd: 0,
                mode: RedirectMode::Read,
                // A non-File target contributes no reference — proves the
                // walker doesn't over-collect from an `Fd` redirect.
                target: ValRedirectTarget::Fd(9),
            }],
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
            wires: vec![Wire::EMPTY, Wire::EMPTY],
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
        let chain = Spanned::synthetic(CompKind::Chain(vec![ret("r_chain_a"), ret("r_chain_b")]));
        let interpolation = Spanned::synthetic(CompKind::Interpolation(vec![
            var("r_interp_a"),
            var("r_interp_b"),
        ]));
        let seq_inner = Spanned::synthetic(CompKind::Seq(vec![ret("r_seq_inner")]));
        let letrec = Spanned::synthetic(CompKind::LetRec {
            slot: None,
            bindings: Arc::new(vec![("letrec_name_bound".to_string(), var("r_letrec_rhs"))]),
        });
        let if_ = Spanned::synthetic(CompKind::If {
            cond: Spanned::synthetic(var("r_if_cond")),
            then: ret("r_if_then"),
            else_: ret("r_if_else"),
        });
        let case = Spanned::synthetic(CompKind::Case {
            scrutinee: Spanned::synthetic(var("r_case_scrutinee")),
            table: Spanned::synthetic(var("r_case_table")),
        });

        let scope_try = Spanned::synthetic(CompKind::Scope(ScopeOp::Try {
            body: var("r_try_body"),
            handler: var("r_try_handler"),
        }));
        let scope_guard = Spanned::synthetic(CompKind::Scope(ScopeOp::Guard {
            body: var("r_guard_body"),
            cleanup: var("r_guard_cleanup"),
        }));
        let scope_within = Spanned::synthetic(CompKind::Scope(ScopeOp::Within {
            opts: var("r_within_opts"),
            body: var("r_within_body"),
        }));
        let scope_grant = Spanned::synthetic(CompKind::Scope(ScopeOp::Grant {
            caps: var("r_grant_caps"),
            body: var("r_grant_body"),
        }));
        let scope_audit = Spanned::synthetic(CompKind::Scope(ScopeOp::Audit {
            body: var("r_audit_body"),
        }));
        let scope_redirect = Spanned::synthetic(CompKind::Scope(ScopeOp::Redirect {
            body: ret("r_scope_redirect_body"),
            redirects: vec![RedirectV {
                fd: 2,
                mode: RedirectMode::Append,
                target: ValRedirectTarget::File(var("r_scope_redirect_target")),
            }],
        }));

        let val_list = Spanned::synthetic(CompKind::Return(Val::List(vec![
            ValListElem::Single(Val::Unit),
            ValListElem::Single(Val::String("s".into())),
            ValListElem::Single(Val::Int(1)),
            ValListElem::Single(Val::Float(1.0)),
            ValListElem::Single(Val::Bool(true)),
            ValListElem::Single(Val::TildePath(TildePath {
                user: None,
                suffix: None,
            })),
            ValListElem::Single(var("r_list_single")),
            ValListElem::Spread(var("r_list_spread")),
        ])));
        let val_map = Spanned::synthetic(CompKind::Return(Val::Map(vec![
            ValMapEntry::Entry(var("r_map_key"), var("r_map_value")),
            ValMapEntry::Spread(var("r_map_spread")),
        ])));
        let val_variant = Spanned::synthetic(CompKind::Return(Val::Variant {
            label: "lbl".into(),
            payload: Some(Box::new(var("r_variant_payload"))),
        }));
        let val_variant_empty = Spanned::synthetic(CompKind::Return(Val::Variant {
            label: "lbl_empty".into(),
            payload: None,
        }));
        let val_thunk = Spanned::synthetic(CompKind::Return(Val::Thunk(Arc::new(
            Spanned::synthetic(CompKind::Return(var("r_thunk_body"))),
        ))));

        let whole = Spanned::synthetic(CompKind::Seq(vec![
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
            Arc::new(chain),
            Arc::new(interpolation),
            Arc::new(seq_inner),
            Arc::new(letrec),
            Arc::new(if_),
            Arc::new(case),
            Arc::new(scope_try),
            Arc::new(scope_guard),
            Arc::new(scope_within),
            Arc::new(scope_grant),
            Arc::new(scope_audit),
            Arc::new(scope_redirect),
            Arc::new(val_list),
            Arc::new(val_map),
            Arc::new(val_variant),
            Arc::new(val_variant_empty),
            Arc::new(val_thunk),
        ]));

        let found: std::collections::HashSet<&str> = referenced_names(&whole).into_iter().collect();

        let expected = [
            "r_force",
            "r_return",
            "r_lam_default",
            "r_lam_body",
            "r_bind_pattern_default",
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
            "r_chain_a",
            "r_chain_b",
            "r_interp_a",
            "r_interp_b",
            "r_seq_inner",
            "r_letrec_rhs",
            "r_if_cond",
            "r_if_then",
            "r_if_else",
            "r_case_scrutinee",
            "r_case_table",
            "r_try_body",
            "r_try_handler",
            "r_guard_body",
            "r_guard_cleanup",
            "r_within_opts",
            "r_within_body",
            "r_grant_caps",
            "r_grant_body",
            "r_audit_body",
            "r_scope_redirect_body",
            "r_scope_redirect_target",
            "r_list_single",
            "r_list_spread",
            "r_map_key",
            "r_map_value",
            "r_map_spread",
            "r_variant_payload",
            "r_thunk_body",
        ];

        for name in expected {
            assert!(found.contains(name), "missing reference: {name}");
        }
        assert_eq!(
            found.len(),
            expected.len(),
            "unexpected extra name in {found:?}; every bound-not-referenced \
             name (lam_param_bound, bind_map_bound, bind_rest_bound, \
             letrec_name_bound) must be absent"
        );

        // Bound names — pattern targets and the LetRec group's own names —
        // must never be treated as references.
        for bound in [
            "lam_param_bound",
            "bind_map_bound",
            "bind_rest_bound",
            "letrec_name_bound",
        ] {
            assert!(
                !found.contains(bound),
                "a bound (not referenced) name leaked into the harvest: {bound}"
            );
        }
    }
}
