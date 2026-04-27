//! Call-by-push-value intermediate representation.
//!
//! The IR is the target of elaboration ([`crate::elaborator`]) and the
//! input to evaluation.  It follows a *call-by-push-value* (CBPV)
//! discipline: [`Val`] is inert data (strings, lists, maps, thunks),
//! [`Comp`] is effectful and sequenced.  This split guarantees that
//! effects are always explicit — a value can never diverge or perform I/O.
//!
//! Every [`Comp`] node carries an optional [`Span`] for error reporting;
//! synthetic nodes (builtins, prelude, generated code) have `span: None`.

use crate::ast::{ExprOp, Param, Pattern, RedirectMode};
use crate::span::Span;
use crate::util::TildePath;

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
pub enum ExecName {
    /// Ordinary unresolved head, subject to alias/builtin/PATH lookup.
    Bare(String),
    /// Slash-bearing literal path, executed directly.
    Path(String),
    /// Tilde-prefixed path, expanded only at the process boundary.
    TildePath(TildePath),
}

impl ExecName {
    pub fn bare(&self) -> Option<&str> {
        match self {
            ExecName::Bare(name) => Some(name),
            ExecName::Path(_) => None,
            ExecName::TildePath(_) => None,
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
    /// A bare word — a textual fragment whose type is inferred by
    /// trying `true`/`false`/`unit` first, then `i64`, then `f64`,
    /// falling back to `String`.  This is the command-line ergonomic
    /// path: `add 2 3` lets `2` and `3` resolve to `Int`.
    Literal(String),
    /// A forced string — produced by single- and double-quoted source.
    /// Always typed `String`; never coerced to `Int`/`Float`/`Bool`.
    /// Quoting is the user's explicit way to defeat the `Literal`
    /// inference cascade (so `'1'` stays a string).
    String(String),
    /// Integer literal from `$[...]` expressions.
    Int(i64),
    /// Floating-point literal from `$[...]` expressions.
    Float(f64),
    /// Boolean literal from `$[...]` expressions.
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
    /// Home-directory expansion: `~`, `~user`, `~/path`, or `~user/path`.
    TildePath(TildePath),
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

/// Target of an I/O redirect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ValRedirectTarget {
    /// Redirect to/from a file path.
    File(Val),
    /// Redirect to/from a file descriptor number.
    Fd(u32),
}

// ── Computations ────────────────────────────────────────────────────────

/// A computation node — effectful, sequenced — with an optional source span.
///
/// This is the primary IR type that the evaluator interprets.  Every node
/// carries its own [`Span`], set once during elaboration, so error messages
/// can point back to the originating source text.  Synthetic nodes
/// (builtins, prelude, generated code) carry `span: None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comp {
    /// Source span of this node, if it originates from user code.
    /// `None` for synthetic nodes (builtins, prelude, generated code).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
    /// The computation proper.
    pub kind: CompKind,
}

impl Comp {
    /// Wrap a `CompKind` with no source span.
    pub fn new(kind: CompKind) -> Self {
        Comp { span: None, kind }
    }

    /// Wrap a `CompKind` with a source span.
    pub fn spanned(span: Span, kind: CompKind) -> Self {
        Comp {
            span: Some(span),
            kind,
        }
    }

    /// Wrap a `CompKind`, attaching `span` if present.  The common case in
    /// the elaborator: the current span is always optional, but the wrap
    /// shape is identical either way.
    pub fn with_span(span: Option<Span>, kind: CompKind) -> Self {
        Comp { span, kind }
    }

    /// True if this computation is a single external/builtin command call.
    /// Used to suppress the ariadne source-span arrow when the entire
    /// input is just one command.
    pub fn is_single_command(&self) -> bool {
        match &self.kind {
            CompKind::Exec { .. } => true,
            CompKind::Seq(stmts) => {
                let mut commands = stmts.iter();
                matches!(
                    commands.next().map(|c| &c.kind),
                    Some(CompKind::Exec { .. })
                ) && commands.next().is_none()
            }
            _ => false,
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
    /// λx. M — a computation that, when evaluated, captures a closure.
    Lam { param: Param, body: Box<Comp> },
    /// rec(x. M) — fixed point: M with x bound to thunk(rec(x. M)).
    Rec { name: String, body: Box<Comp> },
    /// return V — produce a value.
    Return(Val),
    /// M to x. N — sequence: run M, bind result to x, continue with N.
    Bind {
        comp: Box<Comp>,
        pattern: Pattern,
        rest: Box<Comp>,
    },
    /// Application: head is a computation producing a closure, args are values.
    /// Used when the head is a known in-scope binding (as opposed to `Exec`,
    /// which dispatches by name through PATH).
    App {
        head: Box<Comp>,
        args: Vec<Val>,
        redirects: Vec<(u32, RedirectMode, ValRedirectTarget)>,
    },
    /// Command execution by name — the head is an unresolved string looked
    /// up at runtime via the builtin/alias/PATH dispatch chain.
    Exec {
        name: ExecName,
        args: Vec<Val>,
        redirects: Vec<(u32, RedirectMode, ValRedirectTarget)>,
        /// `^name` form: skip aliases/builtins/prelude, go straight to PATH.
        /// Does NOT bypass `within [handlers:]` frames (containment property).
        #[serde(default)]
        external_only: bool,
    },
    /// Direct ral-primitive call.  The name is statically known to refer to
    /// a registered builtin; no alias/PATH lookup is performed.  This is
    /// distinct from `Exec`, which is the effect boundary to outer layers
    /// (aliases, external programs).  Synthesised by `resolve_builtin` so
    /// that `$builtin-name` produces a value pinned to the primitive.
    Builtin { name: String, args: Vec<Val> },
    /// Pipeline: concurrent stages connected by Unix pipes.
    /// Each stage runs in parallel; stdout of stage N feeds stdin of stage N+1.
    Pipeline(Vec<Comp>),
    /// Primitive operation applied to already-evaluated values.  Arises
    /// from elaboration of `$[...]` expressions: `$[a + b]` unfolds to a
    /// `Bind`-sequence with `PrimOp(Add, [Variable(a), Variable(b)])` at
    /// the leaf.  Can fail — division by zero, type mismatch.
    PrimOp(ExprOp, Vec<Val>),
    /// Indexing: V[k1][k2] (can fail — key not found, out of bounds).
    Index { target: Box<Comp>, keys: Vec<Comp> },
    /// Fallback chain (`a ? b ? c`): try each computation in order;
    /// return the first that succeeds.
    Chain(Vec<Comp>),
    /// Background execution (`cmd &`): spawn the computation without
    /// waiting for it to complete.
    Background(Box<Comp>),
    /// String interpolation (effectful — variable lookups can fail).
    Interpolation(Vec<Val>),
    /// Sequence of computations (last value is the result).
    Seq(Vec<Comp>),
    /// Simultaneous fixed point for mutually recursive functions.
    /// slot = None: establish all bindings in the current shell, return Unit.
    /// slot = Some(i): re-establish group in a temporary scope, return lambda for binding i.
    LetRec {
        slot: Option<usize>,
        bindings: Arc<Vec<(String, Comp)>>,
    },
    /// Conditional: evaluate `cond` (must produce `Bool`), then force the
    /// chosen branch thunk.  Both branches have type `U C` for the same `C`;
    /// the result type is `C`.
    If {
        cond: Box<Comp>,
        then: Val,
        else_: Val,
    },
}
