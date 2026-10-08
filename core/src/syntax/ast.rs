//! Abstract syntax tree: the parser's output, the elaborator's input, untyped
//! and shaped exactly like the surface syntax.
//!
//! Spans never sit on [`Ast`] itself. They ride the [`Stmt`] wrapper at every
//! statement position and the inner [`Spanned`] nodes a form carries where a
//! narrower caret is worth having. The elaborator stamps a statement's span as
//! its current position before lowering, so a form with no span of its own
//! inherits the enclosing statement's.

use crate::first_order::Finite;
use crate::ir::{BinaryOp, Pattern, Redirects};
use crate::path::tilde::TildePath;
use crate::source::Spanned;
use crate::syntax::numeral::{self, Shape};
use serde::{Deserialize, Serialize};

/// Unquoted word, shaped once by the lexer. A `/`, or a `~` standing for
/// home, marks it as a path; a path head skips name lookup entirely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Word {
    Plain(String),
    /// `./x`, `/bin/x`
    Slash(String),
    /// `~`, `~/x`
    Tilde(TildePath),
}

impl Word {
    pub(crate) fn as_plain(&self) -> Option<&str> {
        match self {
            Self::Plain(s) => Some(s),
            Self::Slash(_) | Self::Tilde(_) => None,
        }
    }
}

/// One syntactic form.
///
/// The tree is flat — no statement/expression split here; command position,
/// value position, and thunk are read off the surrounding structure by the
/// elaborator and the evaluator. `$[…]` leaves no node of its own: it is the
/// lexical mode in which the operator forms are written, and their operands
/// are ordinary atoms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Ast {
    Word(Word),
    Literal(String),
    /// `$name`
    Variable(String),
    /// `pattern = expr`
    Let {
        pattern: Spanned<Pattern>,
        value: Spanned<Box<Self>>,
    },
    /// `return [<value>]` — the explicit lift from value to command.
    Return(Option<Spanned<Box<Self>>>),
    /// A head applied to arguments. One surface
    /// form, two lowerings: the elaborator emits
    /// [`crate::ir::CompKind::Exec`] for a name it dispatches, and
    /// [`crate::ir::CompKind::App`] when the head resolves to a bound value.
    Call {
        head: Head,
        args: Vec<Spanned<Self>>,
    },
    /// A stage with its redirects, the stage being any of the five forms. One
    /// node for all of them, so the elaborator wraps once: an external command
    /// fuses them into its `Exec`; everything else takes a `Redirect` frame.
    Redirected {
        stage: Spanned<Box<Self>>,
        redirects: Box<Redirects<Self>>,
    },
    /// `try`/`guard`/`within`/`grant`/`audit`. Operand shape is fixed per
    /// [`ScopeAst`] variant; the parser checks arity.
    Scope(ScopeAst),
    /// `cmd1 | cmd2 | cmd3`
    Pipeline(Vec<Stmt>),
    /// `cmd1 ? cmd2 ? cmd3`
    Chain(Vec<Spanned<Self>>),
    /// `{ … }`
    Block(Vec<Stmt>),
    /// `{ |param| … }` — always exactly one parameter; the parser curries the
    /// rest into nested lambdas.
    Lambda {
        param: Spanned<Pattern>,
        body: Vec<Stmt>,
    },
    /// `()` — punctuation that denotes the unit value, like `[]` and `[:]`,
    /// and so not a word.
    Unit,
    /// `[a, b, c]`
    List(Vec<ListElem>),
    /// `[key: val, key: val]` — every key a static label.
    Record(Vec<RecordEntry>),
    /// `[:]`, `["key": val]`, `[$k: val]`: the keys are data.
    Map(Vec<MapEntry>),
    /// `"hello $name"`, one segment per literal fragment or `$…` insertion.
    Interpolation(Vec<Spanned<Self>>),
    /// `` `label `` or `` `label payload ``, where the payload is the next
    /// adjacent atom and `label` drops its backtick.
    Tag {
        label: String,
        payload: Option<Spanned<Box<Self>>>,
    },
    /// The sum eliminator: a scrutinee and one arm per tag of its variant row.
    /// The arms are syntax, not a record the scrutinee is looked up in, so the
    /// alternatives are a finite list the parser hands on whole — which is what
    /// lets the checker prove the set exhaustive and reach inside each arm.
    Case {
        scrutinee: Spanned<Box<Self>>,
        arms: Vec<CaseArm>,
    },
    /// `a op b` inside `$[…]`: arithmetic, ordering, equality.
    Binary(Spanned<Box<Self>>, BinaryOp, Spanned<Box<Self>>),
    /// `-e` inside `$[…]`, strict.
    Negate(Spanned<Box<Self>>),
    /// `not e` inside `$[…]`, strict.
    Not(Spanned<Box<Self>>),
    /// `a && b` — short-circuiting, so the RHS runs only when the LHS is true.
    And(Spanned<Box<Self>>, Spanned<Box<Self>>),
    /// `a || b` — short-circuiting, so the RHS runs only when the LHS is false.
    Or(Spanned<Box<Self>>, Spanned<Box<Self>>),
    /// `target[k1][k2]`; each key's span covers its brackets too.
    Index {
        target: Spanned<Box<Self>>,
        keys: Vec<Spanned<Self>>,
    },
    /// `!atom`; the span covers the `!` along with the operand.
    Force(Spanned<Box<Self>>),
    /// `f ...x`, distinct from [`ListElem::Spread`] so the elaborator can splice
    /// `x`'s elements into the argument list while `f [...x]` stays one list
    /// argument. The parser mints it in argument position and nowhere else.
    Spread(Spanned<Box<Self>>),
    /// `if cond then [elsif cond then]* [else else_]`. The leading `if` and the
    /// `elsif`s collapse into one `branches` vector, being the same thing. With
    /// no `else` the form is Unit; with one, every branch must agree on a type.
    If {
        branches: Vec<IfBranch>,
        else_: Option<Spanned<Box<Self>>>,
    },
}

/// One arm of an [`Ast::Case`]: a literal tag and the computation to run when
/// the scrutinee carries it.
///
/// The *set* of alternatives is syntax; an alternative's body is a function of
/// the payload.  So `body` is the arm's own `{ |p| … }` — an [`Ast::Lambda`] —
/// or a name holding one; elaboration refuses any other atom that would run
/// before the `case` chose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseArm {
    pub tag: Spanned<String>,
    pub(crate) body: Spanned<Box<Ast>>,
}

/// One branch of an [`Ast::If`]: a condition and the body to run when that
/// condition is the first to match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IfBranch {
    pub(crate) cond: Spanned<Box<Ast>>,
    pub(crate) body: Spanned<Box<Ast>>,
}

/// One statement of a sequence: a program, a block body, a lambda body, a
/// pipeline stage.
///
/// The span runs first token to last, not just the keyword, and that matters:
/// an error raised at the outermost `Comp` has nothing narrower to point at,
/// since [`crate::ir::Val`] is unspanned, so the caret falls back to this and
/// must underline the whole statement. Synthetic statements carry no span at
/// all, and the elaborator then keeps the position it already had.
pub(crate) type Stmt = Spanned<Ast>;

/// Parsed command head — a closed category, so nothing downstream has to
/// recover a head's meaning from a generic [`Ast`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Head {
    /// Bare name, subject to value/alias/builtin/PATH lookup.
    Bare(String),
    /// `^name` — external programs only, and exempt from the reserved-word
    /// ban, so `^try` runs the program of that name.
    ExternalName(String),
    /// `./x`, `/bin/x`
    Path(String),
    /// `~/x`; a lone `~` is a value head.
    TildePath(TildePath),
    /// A value head: `$f`, `!$f`, a block literal, `$[…]`, a literal word,
    /// a lone `~`.
    Value(Box<Ast>),
}

/// Element of a list literal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ListElem {
    Single(Spanned<Ast>),
    /// `...expr` — splice `expr`'s elements into this list.
    Spread(Spanned<Ast>),
}

/// Entry of a record literal; no variant can carry a computed key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RecordEntry {
    /// `key: value` or `'key': value` — a label known statically.
    Field {
        key: Spanned<String>,
        value: Spanned<Ast>,
    },
    /// `...expr` — splice another record's fields into this one.
    Spread(Spanned<Ast>),
}

/// Entry of a map literal; a key is data, so no variant can carry a tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MapEntry {
    /// `key: value` — the key a string: a label written out is a literal,
    /// `"…"` and `$name` are computed.
    Entry {
        key: Spanned<Ast>,
        value: Spanned<Ast>,
    },
    /// `...expr` — splice another map's entries into this one.
    Spread(Spanned<Ast>),
}

/// Operand shape of a control-operator scope form, one variant per surface
/// keyword.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ScopeAst {
    /// `try BODY HANDLER` — run `body`; on error, dispatch to `handler`.
    Try { body: Box<Ast>, handler: Box<Ast> },
    /// `guard BODY CLEANUP` — run `body`, then unconditionally `cleanup`.
    Guard { body: Box<Ast>, cleanup: Box<Ast> },
    /// `within OPTS BODY` — install option overrides for the duration of `body`.
    /// `handlers:` is not among `opts`: its labels are the names it binds in
    /// `body`, so it is an arm list the form reads, not data the options carry.
    Within {
        opts: Options,
        handlers: Option<Vec<HandlerArm>>,
        body: Box<Ast>,
    },
    /// `grant CAPS BODY` — attenuate active capabilities across `body`.
    Grant { caps: Options, body: Box<Ast> },
    /// `audit BODY` — run `body` while recording an audit subtree.
    Audit { body: Box<Ast> },
}

/// A form's written options: each label is syntax, each value a term.
pub type Options = Vec<(String, Spanned<Ast>)>;

/// One arm of `within [handlers: …]`: the command name it stands in for, and
/// the value installed under it.
///
/// Arms are syntax, as `case`'s are: the labels are the names bound in the
/// body, so a table assembled elsewhere could never spell them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandlerArm {
    pub name: String,
    pub value: Spanned<Ast>,
}

// ── Utilities ────────────────────────────────────────────────────────────

/// The value-literal shape of a bare word: the one answer to "literal or
/// command name?", read by the parser to skip the [`Ast::Call`] wrapper and
/// by elaboration through [`crate::elaborator::word_val`].
///
/// Purely lexical: the grammar of [`numeral::prefix`] decides, never a round
/// trip through printing.  A float wants a `.`, so `1e5`, which merely happens
/// to f64-parse, stays a string; `007` and `1.50` are numerals all the same, and normalise
/// to `7` and `1.5` wherever they are printed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WordLiteral {
    Bool(bool),
    Int(i64),
    Float(Finite),
}

impl WordLiteral {
    pub(crate) fn classify(s: &str) -> Option<Self> {
        match s {
            "true" => Some(Self::Bool(true)),
            "false" => Some(Self::Bool(false)),
            _ => match numeral::prefix(s)? {
                (n, _) if n != s.len() => None,
                (_, Shape::Int) => s.parse().ok().map(Self::Int),
                // A Float is finite by construction; an overflowing
                // literal like 1.0e999 stays a plain word.
                (_, Shape::Float) => s.parse().ok().and_then(Finite::new).map(Self::Float),
            },
        }
    }
}

impl Ast {
    /// True for the forms that elaborate to a thunk: a `{…}` block or a
    /// `{|p| …}` lambda. `syntax::group` admits only these into a `LetRec`,
    /// since only a thunk can close over a forward reference without the
    /// binding being settled first.
    pub(crate) fn is_thunk_form(&self) -> bool {
        matches!(self, Self::Lambda { .. } | Self::Block(_))
    }

    /// The name and right-hand side of a `let name = rhs`. `None` for anything
    /// else, a destructuring `let [a, b] = …` included: it binds no single name
    /// and so is no `LetRec` member. Both keep their spans: the elaborator
    /// stamps a member with its RHS and refuses a bad binder at its name.
    pub(crate) fn as_name_let(&self) -> Option<(Spanned<&str>, &Spanned<Box<Self>>)> {
        match self {
            Self::Let { pattern, value } => match &pattern.item {
                Pattern::Name(name) => {
                    Some((Spanned::with_span(pattern.span, name.as_ref()), value))
                }
                _ => None,
            },
            _ => None,
        }
    }
}

impl ScopeAst {
    /// Every sub-expression in source order: the operands, plus `within`'s
    /// handler arms, which are syntax and so sit beside the operands rather
    /// than inside one.  Free-variable collection walks them.
    pub(crate) fn operands(&self) -> Vec<&Ast> {
        match self {
            Self::Try { body, handler } => vec![body, handler],
            Self::Guard { body, cleanup } => vec![body, cleanup],
            Self::Within {
                opts,
                handlers,
                body,
            } => {
                let mut ops: Vec<&Ast> = opts.iter().map(|(_, v)| &v.item).collect();
                ops.extend(handlers.iter().flatten().map(|arm| &arm.value.item));
                ops.push(body);
                ops
            }
            Self::Grant { caps, body } => {
                let mut ops: Vec<&Ast> = caps.iter().map(|(_, v)| &v.item).collect();
                ops.push(body);
                ops
            }
            Self::Audit { body } => vec![body],
        }
    }
}
