//! Abstract syntax tree.
//!
//! The AST is produced by the parser and consumed by the elaborator. It is a
//! direct, untyped representation of the surface syntax: commands,
//! pipelines, blocks, lambdas, let-bindings, conditionals, and value
//! literals.
//!
//! # Spans
//!
//! Source spans live on a separate [`Stmt`] wrapper, never on [`Ast`] itself.
//! Every statement position — top-level scripts, block bodies, lambda bodies,
//! and pipeline stages — is a `Vec<Stmt>`, with each `Stmt` carrying the span
//! of its first token.  The elaborator stamps that span as its
//! `current_span` before processing the underlying `Ast`, so diagnostic spans
//! attach at the statement boundary.  Sub-expressions (inside `Call` args,
//! `Let` values, list elements, etc.) inherit the enclosing statement's span
//! through that stamping; the [`Ast::Call`] and [`Ast::Scope`] variants also
//! carry their own spans because their extents differ from the enclosing
//! statement (head + args + redirects vs. a single-statement boundary).
//!
//! The tree is serialisable (via `serde`) for debugging and the `to-json`
//! builtin.

use crate::path::tilde::TildePath;
use crate::source::Spanned;
use crate::syntax::tag::tag_row_label;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Structured unquoted word shape, determined once by the lexer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Word {
    /// Slash-free unquoted word.
    Plain(String),
    /// Slash-bearing unquoted word such as `./x` or `/bin/x`.
    Slash(String),
    /// Tilde-prefixed word such as `~`, `~user`, or `~/x`.
    Tilde(TildePath),
}

impl Word {
    pub fn as_plain(&self) -> Option<&str> {
        match self {
            Word::Plain(s) => Some(s),
            Word::Slash(_) | Word::Tilde(_) => None,
        }
    }
}

/// Top-level AST node. Each variant corresponds to a syntactic form in ral.
///
/// The tree is flat: there is no separate "statement" vs "expression"
/// distinction at this level. The elaborator and evaluator interpret
/// context (command position, value position, thunk, etc.) from the
/// surrounding structure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Ast {
    /// A structured unquoted word.
    Word(Word),
    /// A literal string value.
    Literal(String),
    /// Variable reference: $name
    Variable(String),
    /// Variable binding: pattern = expr.
    ///
    /// The wrapping `Spanned` on `pattern` covers the pattern's parsed
    /// range — used downstream to narrow a pattern-shape diagnostic
    /// (e.g. `let [a, b] = 42` complaining the value isn't a list)
    /// onto the pattern fragment that triggered it.  The wrapping
    /// `Spanned` on `value` covers the right-hand expression's parsed
    /// range so a value-side unify failure underlines the value alone
    /// instead of the entire `let` statement.
    Let {
        pattern: Spanned<Pattern>,
        value: Spanned<Box<Ast>>,
    },
    /// Explicit value-to-command lift: `return [<value>]`.  The
    /// wrapping `Spanned` covers the value expression's parsed range
    /// so a diagnostic that fires while inferring the value (a
    /// heterogeneous list, a wrong-variant payload, …) underlines
    /// that expression rather than the whole `return …` statement.
    Return(Option<Spanned<Box<Ast>>>),
    /// Command-position invocation: a head applied to arguments, with
    /// an optional run of trailing I/O redirects.  At the surface
    /// level this is a single form — the elaborator decides whether
    /// it becomes a name-dispatched [`crate::ir::CompKind::Exec`] (system call)
    /// or a [`crate::ir::CompKind::App`] (CBPV elimination, when the head
    /// resolves to a bound value).
    ///
    /// Each argument is a [`Spanned<Ast>`] so the typechecker can
    /// narrow a per-argument unification failure's caret onto the
    /// offending argument rather than the whole call.  Synthetic
    /// test fixtures use [`Spanned::synthetic`] to elide span tracking.
    Call {
        head: Head,
        args: Vec<Spanned<Ast>>,
        redirects: Vec<Redirect>,
    },
    /// Control-operator scope form (`try`/`guard`/`within`/`grant`/`audit`),
    /// with an optional run of trailing I/O redirects.  Operand shape is
    /// fixed per-variant (see [`ScopeAst`]); the parser validates arity.
    Scope {
        op: ScopeAst,
        redirects: Vec<Redirect>,
    },
    /// A pipeline: cmd1 | cmd2 | cmd3.  Each stage is a [`Stmt`] so the
    /// elaborator can stamp each stage's span before lowering it.
    Pipeline(Vec<Stmt>),
    /// Chained commands: cmd1 ? cmd2 ? cmd3.  Each stage carries its
    /// parsed range so a per-stage diagnostic narrows onto the stage
    /// rather than the whole chain.
    Chain(Vec<Spanned<Ast>>),
    /// Background execution: `command &`.  The `Spanned` covers the
    /// backgrounded expression's parsed range so a diagnostic on the
    /// expression itself underlines just that fragment.
    Background(Spanned<Box<Ast>>),
    /// A block: { ... }.  The body is a statement sequence — see [`Stmt`].
    Block(Vec<Stmt>),
    /// A lambda: { |params| body }.  The body is a statement sequence.
    ///
    /// The wrapping `Spanned` on `param` covers the parameter pattern's
    /// parsed range so a pattern-shape diagnostic at call time narrows
    /// onto the parameter rather than the whole lambda literal.
    Lambda {
        param: Spanned<Param>,
        body: Vec<Stmt>,
    },
    /// A list literal: [a, b, c]
    List(Vec<ListElem>),
    /// A map literal: [key: val, key: val]
    Map(Vec<MapEntry>),
    /// String interpolation: "hello $name".  Each segment (a literal
    /// string fragment or a `$name` / `$[expr]` insertion) carries its
    /// parsed range so a per-segment diagnostic narrows onto the
    /// offending segment rather than the whole interpolation.
    Interpolation(Vec<Spanned<Ast>>),
    /// Variant constructor: `` `label `` (nullary) or `` `label payload `` where the
    /// payload is the next adjacent atom.  The `label` is stored without its
    /// leading backtick.  Tag-keyed record entries are *not* `Ast::Tag` —
    /// they go through `Ast::Map` with [`MapKey::Tag`] keys.  The `Spanned`
    /// on the payload covers the payload expression so a wrong-payload
    /// diagnostic underlines just that fragment.
    Tag {
        label: String,
        payload: Option<Spanned<Box<Ast>>>,
    },
    /// Sum eliminator: `case <scrutinee> [`l₁: h₁, …, `lₙ: hₙ]`.  The
    /// `table` is required to be a tag-keyed record literal whose values
    /// are handler thunks (`{ |x| body }`).  Type-checking and the runtime
    /// connect the scrutinee's variant row to the handler row label by
    /// label.
    ///
    /// Each operand is `Spanned<Box<Ast>>` so the typechecker can
    /// narrow a "case needs a variant" diagnostic onto the scrutinee
    /// expression, and any handler-shape diagnostic onto the table.
    Case {
        scrutinee: Spanned<Box<Ast>>,
        table: Spanned<Box<Ast>>,
    },
    /// Expression block: `$[expr]`
    Expr(Box<Expr>),
    /// Indexing: `$name[k1][k2]`
    ///
    /// `target` carries its own `Spanned` span so a diagnostic that
    /// fires on the target (block-target indexing, etc.) underlines
    /// just the target.  Each key is a [`Spanned<Ast>`] carrying the
    /// byte range the parser read for that `[k]` (including the
    /// surrounding brackets), so a per-key unification failure
    /// underlines the offending key instead of the whole indexing
    /// chain.  Synthetic test fixtures use [`Spanned::synthetic`].
    Index {
        target: Spanned<Box<Ast>>,
        keys: Vec<Spanned<Ast>>,
    },
    /// Force: ! atom.  The wrapping `Spanned` covers the whole
    /// `!atom` extent (the `!` token plus the forced operand), so a
    /// force-on-non-thunk diagnostic underlines just that fragment.
    /// `None` in synthetic test fixtures via [`Spanned::synthetic`].
    Force(Spanned<Box<Ast>>),
    /// Argument-position spread: `f ...x`.  Distinct from
    /// [`ListElem::Spread`] (a list-literal element) so the elaborator
    /// can splice `x`'s elements into the call's argument list while
    /// keeping `f [...x]` as a single list argument.  Only valid as an
    /// immediate child of [`Ast::Call`]'s `args`; elaboration rejects
    /// it elsewhere.  The `Spanned` covers the spread operand so a
    /// "spread of non-list" diagnostic underlines `$x`.
    Spread(Spanned<Box<Ast>>),
    /// Conditional: `if cond then [elsif cond then]* [else else_]`.
    /// One-armed form (single branch, no `else`) has type Unit; multi-
    /// armed form requires every branch and the `else` to agree on
    /// type.
    ///
    /// The leading `if` and any `elsif`s collapse into one `branches`
    /// vector — they are semantically identical (a cond paired with a
    /// body, evaluated in order until one matches).  Each branch
    /// carries spans on both cond and body so a diagnostic on any cond
    /// or any body narrows onto that fragment rather than the whole
    /// `if … else …` form.  `else_` is the optional final body, also
    /// spanned.
    If {
        branches: Vec<IfBranch>,
        else_: Option<Spanned<Box<Ast>>>,
    },
}

/// One branch of an [`Ast::If`]: a condition expression paired with
/// the body to run when that condition is the first to match.  Both
/// halves carry spans so the typechecker can narrow a non-Bool cond
/// or a branch-body type mismatch onto the offending fragment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IfBranch {
    pub cond: Spanned<Box<Ast>>,
    pub body: Spanned<Box<Ast>>,
}

/// One statement in a statement-sequence position (top-level program, block
/// body, lambda body, pipeline stage).  The wrapping `Spanned` carries the
/// statement's span — `start_of_first_token .. end_of_last_token` — so the
/// elaborator can stamp it on emitted IR for diagnostics.  Covering the
/// full extent matters: when an error fires at the outermost `Comp` (e.g.
/// `return [1, hello]` — the offending element has no span of its own
/// because [`crate::ir::Val`] is unspanned), the caret falls back to *this*
/// span, so it needs to underline the whole statement, not just the leading
/// keyword.  Synthetic statements (test fixtures, generated pattern-default
/// wrappers) carry `span: None`; the elaborator's "no narrower position"
/// no-op branch keeps the enclosing position in those cases.
///
/// Statements never appear *inside* sub-expressions: a `Let` value or a
/// `Call` argument is a bare [`Ast`].  The split keeps span overhead on
/// statement boundaries.
pub type Stmt = Spanned<Ast>;

/// Parsed command head.
///
/// This is a closed syntactic category: parser and elaborator do not need to
/// recover head meaning from a generic `Ast`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Head {
    /// Bare command name, subject to value/alias/builtin/PATH lookup.
    Bare(String),
    /// External-only bare command head: `^name`.
    ExternalName(String),
    /// Slash-bearing literal path head such as `./x` or `/bin/x`.
    Path(String),
    /// Tilde path head such as `~/x`.
    TildePath(TildePath),
    /// Any explicit value head (`$f`, `!$f`, block literal, etc.).
    Value(Box<Ast>),
}

/// Binding pattern for `let` and lambda parameters.
///
/// Patterns are irrefutable: they always bind. Wildcard (`_`) discards
/// the value; name binds it; list and map patterns destructure structured
/// values at bind time.
///
/// The type parameter `D` is the form of map-pattern defaults: parser
/// output uses [`Ast`] (the default), IR uses an already-elaborated
/// computation (see [`crate::ir::IrPattern`]).  Lambda parameters and
/// `let` bindings share this single shape; lambda params may carry
/// defaults too via [`Pattern::Map`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Pattern<D = Ast> {
    /// `_` -- discard the value.
    Wildcard,
    /// Bind the value to a name.
    Name(String),
    /// `[a, b, ...rest]` -- destructure a list. The optional `rest`
    /// captures the tail as a new list.
    List {
        elems: Vec<Pattern<D>>,
        rest: Option<String>,
    },
    /// `[key: pat = default, ...]` -- destructure a map. Each entry is
    /// a [`MapPatternEntry`] holding the key, sub-pattern, and optional
    /// default.  The default's representation depends on `D`: surface
    /// AST or elaborated IR.
    Map(Vec<MapPatternEntry<D>>),
}

/// One entry in a [`Pattern::Map`]: a static key, the sub-pattern bound
/// to that field, and an optional default that fires when the key is
/// absent from the value being destructured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MapPatternEntry<D = Ast> {
    pub key: MapKey,
    pub pattern: Pattern<D>,
    pub default: Option<D>,
}

/// Lambda parameter. Always a single pattern; multi-parameter lambdas
/// are desugared by the parser into nested single-parameter lambdas
/// (currying).
pub type Param = Pattern;

/// Element of a list literal. A spread (`...expr`) splices another list
/// into the enclosing one.  Each variant's inner `Spanned<Ast>` covers
/// the element's parsed range so a per-element diagnostic (a
/// heterogeneous-list type mismatch, a spread of a non-list) narrows
/// onto the offending element rather than the whole list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ListElem {
    /// An ordinary element.
    Single(Spanned<Ast>),
    /// `...expr` -- splice the elements of `expr` into this list.
    Spread(Spanned<Ast>),
}

/// Entry of a map literal.  Each variant's value-side `Spanned<Ast>`
/// covers the value expression's parsed range so a per-entry diagnostic
/// (a wrong-shape value, a spread of a non-map) narrows onto the
/// offending value rather than the whole map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MapEntry {
    /// `key: value` with a statically-known label (bare identifier,
    /// quoted string, or backtick tag — encoded in the [`MapKey`]).
    Entry { key: MapKey, value: Spanned<Ast> },
    /// `$name: value` — the key is the runtime value of `name`.
    /// Distinct from [`MapEntry::Entry`] so the surface form is typed
    /// rather than encoded as `Ast::Variable` riding a generic key slot.
    Deref { name: String, value: Spanned<Ast> },
    /// `...expr` — splice another map's entries into this one.
    Spread(Spanned<Ast>),
}

/// Static map / record key — bare identifier or backtick tag.
///
/// Surface `host` and `'host'` both parse to [`MapKey::Bare`]; surface
/// `` `host `` parses to [`MapKey::Tag`] carrying the bare label (no
/// sigil).  The single-string internal row representation (bare label
/// unchanged; tag label prefixed via [`crate::syntax::tag::tag_row_label`]) is
/// produced by [`MapKey::row_label`] when the IR or typechecker needs
/// a row-label `String`.
///
/// The typed key keeps the alphabet readable off the variant rather
/// than by inspecting the leading character of a stringly-typed key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MapKey {
    Bare(String),
    Tag(String),
}

impl MapKey {
    /// Internal row-label representation: bare label unchanged; tag
    /// label prefixed via [`crate::syntax::tag::tag_row_label`].
    pub fn row_label(&self) -> String {
        match self {
            MapKey::Bare(s) => s.clone(),
            MapKey::Tag(label) => tag_row_label(label),
        }
    }

    /// True for tag-alphabet keys.  The parser uses this to enforce
    /// that a single map literal / pattern doesn't mix alphabets.
    pub fn is_tag(&self) -> bool {
        matches!(self, MapKey::Tag(_))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    Integer(i64),
    Number(f64),
    Bool(bool),
    Variable(String),
    /// `$name[k₁][k₂] …` inside `$[…]`.  Name-target only — the surface
    /// syntax doesn't support arbitrary-target indexing here.  Each key
    /// carries its own parsed span so a per-key unification failure
    /// narrows onto the offending key, matching [`Ast::Index`].
    Index(String, Vec<Spanned<Ast>>),
    /// `!atom` inside `$[…]`.  The `Spanned` covers the forced operand
    /// so a force-on-non-thunk diagnostic underlines just the operand,
    /// matching [`Ast::Force`].
    Force(Spanned<Box<Ast>>),
    BinOp(Box<Expr>, BinaryOp, Box<Expr>),
    /// Unary logical negation: `not e` (strict).
    Not(Box<Expr>),
    /// Short-circuit conjunction: `a && b`.  RHS is evaluated only if
    /// LHS is `true`.  Desugars in the elaborator to `_if a { b } { return false }`.
    And(Box<Expr>, Box<Expr>),
    /// Short-circuit disjunction: `a || b`.  RHS is evaluated only if
    /// LHS is `false`.
    Or(Box<Expr>, Box<Expr>),
}

/// Binary primitive operator on values (arithmetic, comparison, equality).
/// The unary `not` lives on its own at [`Expr::Not`] / `CompKind::Not`,
/// so each `BinaryOp` variant is unambiguously two-operand.
///
/// The flat enum is what surfaces on the wire (parser → IR → IPC serde);
/// downstream callers that need to dispatch on the operation *category*
/// use [`BinaryOp::kind`] to project into [`BinaryOpKind`], whose
/// per-category sub-enums make the helpers' invariants type-enforced
/// instead of asserted via `unreachable!`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

/// Arithmetic sub-operations: numeric, may overflow, division and modulo
/// reject a zero divisor, modulo additionally rejects floats.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

/// Ordering sub-operations: numeric only, always return [`bool`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CompareOp {
    Lt,
    Gt,
    Le,
    Ge,
}

/// Equality sub-operations: structural on any value, always return [`bool`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EqOp {
    Eq,
    Ne,
}

/// Category-tagged projection of [`BinaryOp`].  Constructed via
/// [`BinaryOp::kind`].  Dispatching on this rather than the flat enum
/// lets each category's handler accept its own narrowed sub-enum, so
/// the match arms inside are exhaustive without a wildcard fallback.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinaryOpKind {
    Arith(ArithOp),
    Compare(CompareOp),
    Eq(EqOp),
}

impl BinaryOp {
    pub fn kind(self) -> BinaryOpKind {
        match self {
            BinaryOp::Add => BinaryOpKind::Arith(ArithOp::Add),
            BinaryOp::Sub => BinaryOpKind::Arith(ArithOp::Sub),
            BinaryOp::Mul => BinaryOpKind::Arith(ArithOp::Mul),
            BinaryOp::Div => BinaryOpKind::Arith(ArithOp::Div),
            BinaryOp::Mod => BinaryOpKind::Arith(ArithOp::Mod),
            BinaryOp::Lt => BinaryOpKind::Compare(CompareOp::Lt),
            BinaryOp::Gt => BinaryOpKind::Compare(CompareOp::Gt),
            BinaryOp::Le => BinaryOpKind::Compare(CompareOp::Le),
            BinaryOp::Ge => BinaryOpKind::Compare(CompareOp::Ge),
            BinaryOp::Eq => BinaryOpKind::Eq(EqOp::Eq),
            BinaryOp::Ne => BinaryOpKind::Eq(EqOp::Ne),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum RedirectMode {
    Write,
    StreamWrite,
    Append,
    Read,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RedirectTarget {
    File(Box<Ast>),
    Fd(u32),
}

/// I/O redirect attached to a command-position node.  Owned by
/// [`Ast::Call`] and [`Ast::Scope`] as fields rather than mixed into
/// argument lists, so redirects can never be confused with values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Redirect {
    pub fd: u32,
    pub mode: RedirectMode,
    pub target: RedirectTarget,
}

/// Operand shape of a control-operator scope form.  Each variant
/// matches a surface keyword (`try`/`guard`/`within`/`grant`/`audit`);
/// arity, operand description, and constructor-from-operands are
/// declared together in [`ScopeAst::KEYWORDS`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ScopeAst {
    /// `try BODY HANDLER` — run `body`; on error, dispatch to `handler`.
    Try { body: Box<Ast>, handler: Box<Ast> },
    /// `guard BODY CLEANUP` — run `body`, then unconditionally `cleanup`.
    Guard { body: Box<Ast>, cleanup: Box<Ast> },
    /// `within OPTS BODY` — install option overrides for the duration of `body`.
    Within { opts: Box<Ast>, body: Box<Ast> },
    /// `grant CAPS BODY` — attenuate active capabilities across `body`.
    Grant { caps: Box<Ast>, body: Box<Ast> },
    /// `audit BODY` — run `body` while recording an audit subtree.
    Audit { body: Box<Ast> },
}

/// Parser metadata for one control-operator keyword: the surface
/// name, its operand arity, a human-readable operand description for
/// arity-mismatch diagnostics, and a constructor that destructures
/// the arity-validated operand vector into the matching [`ScopeAst`].
///
/// Kept next to [`ScopeAst`] so each entry holds everything the
/// parser needs to recognise the keyword in one place; the parser
/// dispatches by name through [`ScopeAst::lookup_keyword`].
pub struct ScopeKeyword {
    pub name: &'static str,
    pub arity: usize,
    pub operand_desc: &'static str,
    pub build: fn(Vec<Ast>) -> ScopeAst,
}

impl ScopeAst {
    /// All recognised control-operator keywords.  The parser's
    /// `is_reserved` predicate consults [`Self::lookup_keyword`] on
    /// this list to bar these names from binding positions.
    pub const KEYWORDS: &'static [ScopeKeyword] = &[
        ScopeKeyword {
            name: "try",
            arity: 2,
            operand_desc: "body, handler",
            build: |ops| {
                let [body, handler]: [Ast; 2] = ops.try_into().expect("arity validated");
                ScopeAst::Try {
                    body: Box::new(body),
                    handler: Box::new(handler),
                }
            },
        },
        ScopeKeyword {
            name: "guard",
            arity: 2,
            operand_desc: "body, cleanup",
            build: |ops| {
                let [body, cleanup]: [Ast; 2] = ops.try_into().expect("arity validated");
                ScopeAst::Guard {
                    body: Box::new(body),
                    cleanup: Box::new(cleanup),
                }
            },
        },
        ScopeKeyword {
            name: "within",
            arity: 2,
            operand_desc: "options, body",
            build: |ops| {
                let [opts, body]: [Ast; 2] = ops.try_into().expect("arity validated");
                ScopeAst::Within {
                    opts: Box::new(opts),
                    body: Box::new(body),
                }
            },
        },
        ScopeKeyword {
            name: "grant",
            arity: 2,
            operand_desc: "capabilities, body",
            build: |ops| {
                let [caps, body]: [Ast; 2] = ops.try_into().expect("arity validated");
                ScopeAst::Grant {
                    caps: Box::new(caps),
                    body: Box::new(body),
                }
            },
        },
        ScopeKeyword {
            name: "audit",
            arity: 1,
            operand_desc: "body",
            build: |ops| {
                let [body]: [Ast; 1] = ops.try_into().expect("arity validated");
                ScopeAst::Audit {
                    body: Box::new(body),
                }
            },
        },
    ];

    /// Look up a control-operator keyword by surface name.  Returns
    /// `None` if `name` is not a recognised keyword.
    pub fn lookup_keyword(name: &str) -> Option<&'static ScopeKeyword> {
        Self::KEYWORDS.iter().find(|kw| kw.name == name)
    }
}

// ── Utilities ────────────────────────────────────────────────────────────

/// Syntactic classification of a bare-word string into a value-literal
/// shape.  The single source of truth for "does this bare word look like
/// a literal rather than a command name?" — used at parse time to skip
/// the [`Ast::Call`] wrapper, and again at elaboration to choose the
/// appropriate typed [`crate::ir::Val`] variant (see
/// [`crate::ir::Val::from_word`]).
///
/// Floats require an embedded `.` to disambiguate from identifiers that
/// happen to f64-parse (e.g. `1e5` stays a `String`).
#[derive(Debug, Clone, PartialEq)]
pub enum WordLiteral {
    Bool(bool),
    Unit,
    Int(i64),
    Float(f64),
}

impl WordLiteral {
    pub fn classify(s: &str) -> Option<WordLiteral> {
        match s {
            "true" => Some(WordLiteral::Bool(true)),
            "false" => Some(WordLiteral::Bool(false)),
            "unit" => Some(WordLiteral::Unit),
            _ if s.parse::<i64>().is_ok() => s.parse().ok().map(WordLiteral::Int),
            _ if s.contains('.') => s.parse().ok().map(WordLiteral::Float),
            _ => None,
        }
    }
}

impl<D> Pattern<D> {
    pub fn collect_names(&self, set: &mut HashSet<String>) {
        match self {
            Pattern::Wildcard => {}
            Pattern::Name(n) => {
                set.insert(n.clone());
            }
            Pattern::List { elems, rest } => {
                for e in elems {
                    e.collect_names(set);
                }
                if let Some(r) = rest {
                    set.insert(r.clone());
                }
            }
            Pattern::Map(entries) => {
                for entry in entries {
                    entry.pattern.collect_names(set);
                }
            }
        }
    }
}

impl Ast {
    /// True for AST nodes that elaborate to a thunk value — a `{…}`
    /// block (nullary thunk) or a `{|param| …}` lambda (closure).
    /// Used by `group.rs` to decide which `let` RHS expressions can
    /// participate in a `LetRec`: only those whose RHS is itself a
    /// thunk value can close over forward references without
    /// requiring the binding to be settled first.
    pub fn is_thunk_form(&self) -> bool {
        matches!(self, Ast::Lambda { .. } | Ast::Block(_))
    }
}

impl ScopeAst {
    /// Operands of this scope form, in source order (e.g. `try BODY
    /// HANDLER` → `[body, handler]`).  Mirrors the per-variant arity
    /// declared in [`Self::KEYWORDS`]; consumed by free-variable
    /// collection.
    pub fn operands(&self) -> Vec<&Ast> {
        match self {
            ScopeAst::Try { body, handler } => vec![body, handler],
            ScopeAst::Guard { body, cleanup } => vec![body, cleanup],
            ScopeAst::Within { opts, body } => vec![opts, body],
            ScopeAst::Grant { caps, body } => vec![caps, body],
            ScopeAst::Audit { body } => vec![body],
        }
    }
}
