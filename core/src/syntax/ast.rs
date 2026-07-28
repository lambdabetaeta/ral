//! Abstract syntax tree: the parser's output, the elaborator's input, untyped
//! and shaped exactly like the surface syntax.
//!
//! Spans never sit on [`Ast`] itself. They ride the [`Stmt`] wrapper at every
//! statement position and the inner [`Spanned`] nodes a form carries where a
//! narrower caret is worth having. The elaborator stamps a statement's span as
//! its current position before lowering, so a form with no span of its own
//! inherits the enclosing statement's.

use crate::path::tilde::TildePath;
use crate::source::Spanned;
use crate::syntax::tag::tag_row_label;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Unquoted word, shaped once by the lexer. A leading slash or tilde marks it
/// as a path, and in head position that skips name lookup entirely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Word {
    Plain(String),
    /// `./x`, `/bin/x`
    Slash(String),
    /// `~`, `~user`, `~/x`
    Tilde(TildePath),
}

impl Word {
    pub fn as_plain(&self) -> Option<&str> {
        match self {
            Self::Plain(s) => Some(s),
            Self::Slash(_) | Self::Tilde(_) => None,
        }
    }
}

/// One syntactic form. The tree is flat — no statement/expression split here;
/// command position, value position, and thunk are read off the surrounding
/// structure by the elaborator and the evaluator.
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
    /// A head applied to arguments, plus any trailing redirects. One surface
    /// form, two lowerings: the elaborator emits
    /// [`crate::ir::CompKind::Exec`] for a name it dispatches, and
    /// [`crate::ir::CompKind::App`] when the head resolves to a bound value.
    Call {
        head: Head,
        args: Vec<Spanned<Self>>,
        redirects: Vec<Redirect>,
    },
    /// `try`/`guard`/`within`/`grant`/`audit`, plus any trailing redirects.
    /// Operand shape is fixed per [`ScopeAst`] variant; the parser checks arity.
    Scope {
        op: ScopeAst,
        redirects: Vec<Redirect>,
    },
    /// `cmd1 | cmd2 | cmd3`
    Pipeline(Vec<Stmt>),
    /// `cmd1 ? cmd2 ? cmd3`
    Chain(Vec<Spanned<Self>>),
    /// `command &`
    Background(Spanned<Box<Self>>),
    /// `{ … }`
    Block(Vec<Stmt>),
    /// `{ |param| … }` — always exactly one parameter; the parser curries the
    /// rest into nested lambdas.
    Lambda {
        param: Spanned<Param>,
        body: Vec<Stmt>,
    },
    /// `[a, b, c]`
    List(Vec<ListElem>),
    /// `[key: val, key: val]`
    Map(Vec<MapEntry>),
    /// `"hello $name"`, one segment per literal fragment or `$…` insertion.
    Interpolation(Vec<Spanned<Self>>),
    /// `` `label `` or `` `label payload ``, where the payload is the next
    /// adjacent atom and `label` drops its backtick. Tag-*keyed* records are
    /// not this: they are `Map` entries with [`MapKey::Tag`] keys.
    Tag {
        label: String,
        payload: Option<Spanned<Box<Self>>>,
    },
    /// The sum eliminator, matching the scrutinee's variant row against the
    /// table's handler row label by label. Both operands parse as bare atoms;
    /// only the typechecker insists on a variant and a tag-keyed record of
    /// handler thunks, so its complaint can name the resolved types.
    Case {
        scrutinee: Spanned<Box<Self>>,
        table: Spanned<Box<Self>>,
    },
    /// `$[expr]`
    Expr(Box<Expr>),
    /// `$name[k1][k2]`; each key's span covers its brackets too.
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

/// One branch of an [`Ast::If`]: a condition and the body to run when that
/// condition is the first to match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IfBranch {
    pub cond: Spanned<Box<Ast>>,
    pub body: Spanned<Box<Ast>>,
}

/// One statement of a sequence: a program, a block body, a lambda body, a
/// pipeline stage.
///
/// The span runs first token to last, not just the keyword, and that matters:
/// an error raised at the outermost `Comp` has nothing narrower to point at,
/// since [`crate::ir::Val`] is unspanned, so the caret falls back to this and
/// must underline the whole statement. Synthetic statements carry no span at
/// all, and the elaborator then keeps the position it already had.
pub type Stmt = Spanned<Ast>;

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
    /// `~/x`
    TildePath(TildePath),
    /// An explicit value head: `$f`, `!$f`, a block literal.
    Value(Box<Ast>),
}

/// Binding pattern, shared by `let` and lambda parameters. There is no
/// alternative to fall through to, so a shape mismatch at bind time is an
/// error rather than a failure to match.
///
/// `D` is the shape of map-pattern defaults: surface [`Ast`] from the parser,
/// an already-elaborated computation in [`crate::ir::IrPattern`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Pattern<D = Ast> {
    /// `_` — discard the value.
    Wildcard,
    Name(String),
    /// `[a, b, ...rest]`, where `rest` takes the tail as a new list.
    List {
        elems: Vec<Self>,
        rest: Option<String>,
    },
    /// `[key: pat = default, …]`
    Map(Vec<MapPatternEntry<D>>),
}

/// One entry of a [`Pattern::Map`]: a static key, the sub-pattern bound to that
/// field, and a default that fires when the key is absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MapPatternEntry<D = Ast> {
    pub key: MapKey,
    pub pattern: Pattern<D>,
    pub default: Option<D>,
}

/// Lambda parameter.
pub type Param = Pattern;

/// Element of a list literal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ListElem {
    Single(Spanned<Ast>),
    /// `...expr` — splice `expr`'s elements into this list.
    Spread(Spanned<Ast>),
}

/// Entry of a map literal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MapEntry {
    /// `key: value` with a label known statically — see [`MapKey`].
    Entry { key: MapKey, value: Spanned<Ast> },
    /// `$name: value` — the key is `name`'s value at runtime.
    Deref { name: String, value: Spanned<Ast> },
    /// `...expr` — splice another map's entries into this one.
    Spread(Spanned<Ast>),
}

/// Static record key. Both `host` and `'host'` parse to [`MapKey::Bare`];
/// `` `host `` parses to [`MapKey::Tag`] carrying the label without its sigil.
/// Holding the alphabet on the variant spares every reader from sniffing the
/// leading character of a stringly-typed key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MapKey {
    Bare(String),
    Tag(String),
}

impl MapKey {
    /// The single-string row label the IR and typechecker want: bare unchanged,
    /// tag prefixed by [`crate::syntax::tag::tag_row_label`].
    pub fn row_label(&self) -> String {
        match self {
            Self::Bare(s) => s.clone(),
            Self::Tag(label) => tag_row_label(label),
        }
    }

    /// True for tag-alphabet keys. The parser reads it to bar a single map
    /// literal or pattern from mixing the two alphabets.
    pub fn is_tag(&self) -> bool {
        matches!(self, Self::Tag(_))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    Integer(i64),
    Number(f64),
    Bool(bool),
    Variable(String),
    /// `$name[k₁][k₂] …` inside `$[…]`. Name targets only — unlike
    /// [`Ast::Index`], the surface syntax admits no arbitrary target here.
    Index(String, Vec<Spanned<Ast>>),
    /// `!atom` inside `$[…]`; the span covers only the operand.
    Force(Spanned<Box<Ast>>),
    BinOp(Box<Self>, BinaryOp, Box<Self>),
    /// `-e`, strict.
    Negate(Box<Self>),
    /// `not e`, strict.
    Not(Box<Self>),
    /// `a && b` — short-circuiting, so the RHS runs only when the LHS is true.
    And(Box<Self>, Box<Self>),
    /// `a || b` — short-circuiting, so the RHS runs only when the LHS is false.
    Or(Box<Self>, Box<Self>),
}

/// Binary primitive on values: arithmetic, ordering, equality. The flat enum is
/// what crosses the wire, parser to IR to IPC; a caller that wants to dispatch
/// on category projects it through [`BinaryOp::kind`] into [`BinaryOpKind`],
/// whose sub-enums let each handler match exhaustively without a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Numeric, and may overflow. Division and modulo reject a zero divisor;
/// modulo also rejects floats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

/// Numeric operands only; always a [`bool`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Lt,
    Gt,
    Le,
    Ge,
}

/// Structural on any value; always a [`bool`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EqOp {
    Eq,
    Ne,
}

/// Category-tagged projection of [`BinaryOp`], built by [`BinaryOp::kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOpKind {
    Arith(ArithOp),
    Compare(CompareOp),
    Eq(EqOp),
}

impl BinaryOp {
    pub fn kind(self) -> BinaryOpKind {
        match self {
            Self::Add => BinaryOpKind::Arith(ArithOp::Add),
            Self::Sub => BinaryOpKind::Arith(ArithOp::Sub),
            Self::Mul => BinaryOpKind::Arith(ArithOp::Mul),
            Self::Div => BinaryOpKind::Arith(ArithOp::Div),
            Self::Mod => BinaryOpKind::Arith(ArithOp::Mod),
            Self::Lt => BinaryOpKind::Compare(CompareOp::Lt),
            Self::Gt => BinaryOpKind::Compare(CompareOp::Gt),
            Self::Le => BinaryOpKind::Compare(CompareOp::Le),
            Self::Ge => BinaryOpKind::Compare(CompareOp::Ge),
            Self::Eq => BinaryOpKind::Eq(EqOp::Eq),
            Self::Ne => BinaryOpKind::Eq(EqOp::Ne),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RedirectMode {
    Write,
    StreamWrite,
    Append,
    Read,
    /// `<< str` — feed a string to stdin. The target word is the payload
    /// itself, not a path, and one leading newline is dropped at evaluation so
    /// a multiline body may start on the line below the command.
    HereString,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RedirectTarget {
    /// A file path, or the payload for [`RedirectMode::HereString`].
    File(Box<Ast>),
    Fd(u32),
}

/// An I/O redirect. It is a field of [`Ast::Call`] and [`Ast::Scope`] rather
/// than an entry in their argument lists, so it can never pass for a value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Redirect {
    pub fd: u32,
    pub mode: RedirectMode,
    pub target: RedirectTarget,
}

/// Operand shape of a control-operator scope form, one variant per surface
/// keyword. Arity and construction are declared in [`ScopeAst::KEYWORDS`].
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

/// Everything the parser needs for one control-operator keyword: the surface
/// name, the operand arity, a description of the operands for the
/// arity-mismatch message, and a constructor from the validated operand vector.
pub struct ScopeKeyword {
    pub name: &'static str,
    pub arity: usize,
    pub operand_desc: &'static str,
    pub build: fn(Vec<Ast>) -> ScopeAst,
}

impl ScopeAst {
    /// Every control-operator keyword. [`crate::syntax::is_keyword`] reads this
    /// list, so the parser's ban on these names in binding positions and
    /// exarch's syntax highlighter cannot drift apart.
    pub const KEYWORDS: &'static [ScopeKeyword] = &[
        ScopeKeyword {
            name: "try",
            arity: 2,
            operand_desc: "body, handler",
            build: |ops| {
                let [body, handler]: [Ast; 2] = ops.try_into().expect("arity validated");
                Self::Try {
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
                Self::Guard {
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
                Self::Within {
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
                Self::Grant {
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
                Self::Audit {
                    body: Box::new(body),
                }
            },
        },
    ];

    /// Look up a control-operator keyword by surface name.
    pub fn lookup_keyword(name: &str) -> Option<&'static ScopeKeyword> {
        Self::KEYWORDS.iter().find(|kw| kw.name == name)
    }
}

// ── Utilities ────────────────────────────────────────────────────────────

/// The value-literal shape of a bare word: the one answer to "literal or
/// command name?", read by the parser to skip the [`Ast::Call`] wrapper and by
/// elaboration through [`crate::ir::Val::from_word`]. A float wants an embedded
/// `.`, so `1e5`, which merely happens to f64-parse, stays a string.
#[derive(Debug, Clone, PartialEq)]
pub enum WordLiteral {
    Bool(bool),
    Unit,
    Int(i64),
    Float(f64),
}

impl WordLiteral {
    pub fn classify(s: &str) -> Option<Self> {
        match s {
            "true" => Some(Self::Bool(true)),
            "false" => Some(Self::Bool(false)),
            "unit" => Some(Self::Unit),
            _ => {
                if let Ok(i) = s.parse::<i64>() {
                    Some(Self::Int(i))
                } else if s.contains('.') {
                    s.parse().ok().map(Self::Float)
                } else {
                    None
                }
            }
        }
    }
}

impl<D> Pattern<D> {
    pub fn collect_names(&self, set: &mut HashSet<String>) {
        match self {
            Self::Wildcard => {}
            Self::Name(n) => {
                set.insert(n.clone());
            }
            Self::List { elems, rest } => {
                for e in elems {
                    e.collect_names(set);
                }
                if let Some(r) = rest {
                    set.insert(r.clone());
                }
            }
            Self::Map(entries) => {
                for entry in entries {
                    entry.pattern.collect_names(set);
                }
            }
        }
    }
}

impl Ast {
    /// True for the forms that elaborate to a thunk: a `{…}` block or a
    /// `{|p| …}` lambda. `syntax::group` admits only these into a `LetRec`,
    /// since only a thunk can close over a forward reference without the
    /// binding being settled first.
    pub fn is_thunk_form(&self) -> bool {
        matches!(self, Self::Lambda { .. } | Self::Block(_))
    }

    /// The name and right-hand side of a `let name = rhs`. `None` for anything
    /// else, a destructuring `let [a, b] = …` included: it binds no single name
    /// and so is neither a `LetRec` member nor a worksheet node. The [`Spanned`]
    /// survives because `syntax::group` wants the RHS span.
    pub fn as_name_let(&self) -> Option<(&str, &Spanned<Box<Self>>)> {
        match self {
            Self::Let { pattern, value } => match &pattern.item {
                Pattern::Name(name) => Some((name.as_str(), value)),
                _ => None,
            },
            _ => None,
        }
    }
}

impl ScopeAst {
    /// Operands in source order, matching the arity in [`Self::KEYWORDS`].
    /// Free-variable collection walks them.
    pub fn operands(&self) -> Vec<&Ast> {
        match self {
            Self::Try { body, handler } => vec![body, handler],
            Self::Guard { body, cleanup } => vec![body, cleanup],
            Self::Within { opts, body } => vec![opts, body],
            Self::Grant { caps, body } => vec![caps, body],
            Self::Audit { body } => vec![body],
        }
    }
}
