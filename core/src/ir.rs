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
            CommandName::Bare(name) => Some(name),
            CommandName::Path(_) => None,
            CommandName::TildePath(_) => None,
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
        payload: Option<Box<Val>>,
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
    pub fn from_word(s: &str) -> Val {
        use crate::syntax::ast::WordLiteral;
        match WordLiteral::classify(s) {
            Some(WordLiteral::Bool(b)) => Val::Bool(b),
            Some(WordLiteral::Unit) => Val::Unit,
            Some(WordLiteral::Int(n)) => Val::Int(n),
            Some(WordLiteral::Float(f)) => Val::Float(f),
            None => Val::String(s.to_string()),
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
            CommandWord::Name(n) | CommandWord::External(n) => n,
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
    use super::Val;

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
}
