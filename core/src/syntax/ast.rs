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
use crate::syntax::lexer::RedirectOp;
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
    /// A head applied to arguments, plus any trailing redirects. One surface
    /// form, two lowerings: the elaborator emits
    /// [`crate::ir::CompKind::Exec`] for a name it dispatches, and
    /// [`crate::ir::CompKind::App`] when the head resolves to a bound value.
    Call {
        head: Head,
        args: Vec<Spanned<Self>>,
        redirects: Vec<Redirect<Self>>,
    },
    /// `try`/`guard`/`within`/`grant`/`audit`, plus any trailing redirects.
    /// Operand shape is fixed per [`ScopeAst`] variant; the parser checks arity.
    Scope {
        op: ScopeAst,
        redirects: Vec<Redirect<Self>>,
    },
    /// `cmd1 | cmd2 | cmd3`
    Pipeline(Vec<Stmt>),
    /// `cmd1 ? cmd2 ? cmd3`
    Chain(Vec<Spanned<Self>>),
    /// `{ … }`
    Block(Vec<Stmt>),
    /// `{ |param| … }` — always exactly one parameter; the parser curries the
    /// rest into nested lambdas.
    Lambda {
        param: Spanned<Param>,
        body: Vec<Stmt>,
    },
    /// `()` — punctuation that denotes the unit value, like `[]` and `[:]`,
    /// and so not a word.
    Unit,
    /// `[a, b, c]`
    List(Vec<ListElem>),
    /// `[key: val, key: val]` — every key a static label.
    Record(Vec<RecordEntry>),
    /// `[:]`, `[:, key: val]`, `[$k: val]` — the keys are data.
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
/// The *set* of alternatives is syntax; an alternative's body is a
/// computation, however it is spelled. So `body` is the arm's own
/// `{ |p| … }` — an [`Ast::Lambda`] — or any other atom, which elaboration
/// applies to the payload.
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
    /// `~/x`
    TildePath(TildePath),
    /// An explicit value head: `$f`, `!$f`, a block literal.
    Value(Box<Ast>),
}

/// Binding pattern, shared by `let` and lambda parameters. There is no
/// alternative to fall through to, so a shape mismatch at bind time is an
/// error rather than a failure to match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Pattern {
    /// `_` — discard the value.
    Wildcard,
    Name(String),
    /// `[a, b, ...rest]`, where `rest` takes the tail as a new list.
    List {
        elems: Vec<Self>,
        rest: Option<String>,
    },
    /// `[key: pat, …]`
    Map(Vec<MapPatternEntry>),
}

/// One entry of a [`Pattern::Map`]: a static key and the sub-pattern bound to
/// that field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MapPatternEntry {
    pub(crate) key: String,
    pub(crate) pattern: Pattern,
}

/// Lambda parameter.
pub(crate) type Param = Pattern;

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
    Field { key: String, value: Spanned<Ast> },
    /// `...expr` — splice another record's fields into this one.
    Spread(Spanned<Ast>),
}

/// Entry of a map literal; a key is data, so no variant can carry a tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MapEntry {
    /// `key: value` or `'key': value` — written out, but still data.
    Entry { key: String, value: Spanned<Ast> },
    /// `$name: value` — the key is `name`'s value at runtime.
    Deref { name: String, value: Spanned<Ast> },
    /// `...expr` — splice another map's entries into this one.
    Spread(Spanned<Ast>),
}

/// Binary primitive on values: arithmetic, ordering, equality.
///
/// The flat enum is what crosses the wire, parser to IR to IPC; a caller that
/// wants to dispatch on category projects it through [`BinaryOp::kind`] into
/// [`BinaryOpKind`], whose sub-enums let each handler match exhaustively
/// without a wildcard arm.
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

/// How a write redirect opens its file: `>` replaces it atomically, `>>`
/// appends, `>~` truncates and streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WriteMode {
    Write,
    Append,
    Stream,
}

/// What `<` or `<<` feeds standard input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StdinSource<T> {
    /// `< path`.
    File(T),
    /// `<< str`: the payload itself.  One leading newline is dropped at
    /// evaluation, so a multiline body may start on the line below.
    Here(T),
}

/// An I/O redirect onto one of ral's three streams.
///
/// Its operand `T` is the parsed word, then the elaborated value, then the
/// evaluated string.  A field of [`Ast::Call`] and [`Ast::Scope`] rather than
/// an argument, so it can never pass for a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Redirect<T> {
    Stdin(StdinSource<T>),
    Stdout(WriteMode, T),
    Stderr(WriteMode, T),
    /// `2>&1`.
    StderrToStdout,
}

impl<T> Redirect<T> {
    /// Eliminate a word-taking redirect token into the three streams.  ral
    /// has no fd plumbing beyond them, so any other fd is refused rather
    /// than reinterpreted.  (The lexer refuses fd ≥ 3 first, with advice
    /// this rule cannot give; the last arm is the rule stated whole.)
    pub(crate) fn word(fd: Option<u32>, op: RedirectOp, word: T) -> Result<Self, String> {
        let fd = fd.unwrap_or(match op {
            RedirectOp::Write(_) => 1,
            RedirectOp::Read | RedirectOp::HereString => 0,
        });
        match (fd, op) {
            (0, RedirectOp::Read) => Ok(Self::Stdin(StdinSource::File(word))),
            (0, RedirectOp::HereString) => Ok(Self::Stdin(StdinSource::Here(word))),
            (1, RedirectOp::Write(mode)) => Ok(Self::Stdout(mode, word)),
            (2, RedirectOp::Write(mode)) => Ok(Self::Stderr(mode, word)),
            (_, RedirectOp::HereString) => {
                Err("`<<` always feeds stdin — drop the file-descriptor prefix".into())
            }
            (_, RedirectOp::Read) => Err(format!(
                "`<` always feeds standard input, so `{fd}<` reads nothing in ral — \
                 drop the `{fd}`, or did you mean `{fd}> file` to write there?"
            )),
            (0, RedirectOp::Write(_)) => Err(STDIN_UNWRITABLE.into()),
            (_, RedirectOp::Write(_)) => Err(format!(
                "file descriptor {fd}: ral has only standard input (0), standard output (1) \
                 and standard error (2)"
            )),
        }
    }

    /// Eliminate `fd>&to`.  `2>&1` is the one dup ral models; the identity
    /// dups `1>&1` and `2>&2` name the stream they already are, and denote
    /// no redirect at all.
    pub(crate) fn dup(fd: Option<u32>, to: u32) -> Result<Option<Self>, String> {
        match (fd.unwrap_or(1), to) {
            (2, 1) => Ok(Some(Self::StderrToStdout)),
            (1, 1) | (2, 2) => Ok(None),
            (0, _) => Err(STDIN_UNWRITABLE.into()),
            (fd, to) => Err(format!(
                "ral has no fd plumbing beyond `2>&1`, so `{fd}>&{to}` has nothing to mean"
            )),
        }
    }

    pub(crate) fn operand(&self) -> Option<&T> {
        match self {
            Self::Stdin(StdinSource::File(t) | StdinSource::Here(t))
            | Self::Stdout(_, t)
            | Self::Stderr(_, t) => Some(t),
            Self::StderrToStdout => None,
        }
    }

    pub(crate) fn try_map<U, E>(
        &self,
        mut f: impl FnMut(&T) -> Result<U, E>,
    ) -> Result<Redirect<U>, E> {
        Ok(match self {
            Self::Stdin(StdinSource::File(t)) => Redirect::Stdin(StdinSource::File(f(t)?)),
            Self::Stdin(StdinSource::Here(t)) => Redirect::Stdin(StdinSource::Here(f(t)?)),
            Self::Stdout(mode, t) => Redirect::Stdout(*mode, f(t)?),
            Self::Stderr(mode, t) => Redirect::Stderr(*mode, f(t)?),
            Self::StderrToStdout => Redirect::StderrToStdout,
        })
    }

    pub(crate) fn map<U>(&self, mut f: impl FnMut(&T) -> U) -> Redirect<U> {
        let Ok(r) = self.try_map(|t| Ok::<_, std::convert::Infallible>(f(t)));
        r
    }
}

const STDIN_UNWRITABLE: &str =
    "standard input cannot be written to — did you mean `< file`, which reads one into it?";

/// Operand shape of a control-operator scope form, one variant per surface
/// keyword. Arity and construction are declared in [`ScopeAst::KEYWORDS`].
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
        opts: Box<Ast>,
        handlers: Option<Vec<HandlerArm>>,
        body: Box<Ast>,
    },
    /// `grant CAPS BODY` — attenuate active capabilities across `body`.
    Grant { caps: Box<Ast>, body: Box<Ast> },
    /// `audit BODY` — run `body` while recording an audit subtree.
    Audit { body: Box<Ast> },
}

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

/// How a control operator reads the operand at one position.
///
/// Not every operand is an expression: a form's option bracket is the form's
/// own syntax, where `[]` is the empty option set rather than a list.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operand {
    Atom,
    /// `[]`, `[l: v, …]`, `[...b, l: v, …]`, or an atom naming a bundle.
    /// `arms` marks the form whose options include the `handlers:` arm list.
    Options {
        arms: bool,
    },
}

/// Everything the parser needs for one control-operator keyword.
///
/// The surface name, how each operand position is read, a description of the
/// operands for the arity-mismatch message, and a constructor from the
/// validated operands and whatever arm list an option bracket lifted out.
pub(crate) struct ScopeKeyword {
    pub name: &'static str,
    pub(crate) operands: &'static [Operand],
    pub(crate) operand_desc: &'static str,
    pub(crate) build: fn(Vec<Ast>, Option<Vec<HandlerArm>>) -> ScopeAst,
}

impl ScopeKeyword {
    pub(crate) fn arity(&self) -> usize {
        self.operands.len()
    }
}

impl ScopeAst {
    /// Every control-operator keyword. [`crate::syntax::is_keyword`] reads this
    /// list, so the parser's ban on these names in binding positions and
    /// exarch's syntax highlighter cannot drift apart.
    pub(crate) const KEYWORDS: &'static [ScopeKeyword] = &[
        ScopeKeyword {
            name: "try",
            operands: &[Operand::Atom, Operand::Atom],
            operand_desc: "body, handler",
            build: |ops, _| {
                let [body, handler]: [Ast; 2] = ops.try_into().expect("arity validated");
                Self::Try {
                    body: Box::new(body),
                    handler: Box::new(handler),
                }
            },
        },
        ScopeKeyword {
            name: "guard",
            operands: &[Operand::Atom, Operand::Atom],
            operand_desc: "body, cleanup",
            build: |ops, _| {
                let [body, cleanup]: [Ast; 2] = ops.try_into().expect("arity validated");
                Self::Guard {
                    body: Box::new(body),
                    cleanup: Box::new(cleanup),
                }
            },
        },
        ScopeKeyword {
            name: "within",
            operands: &[Operand::Options { arms: true }, Operand::Atom],
            operand_desc: "options, body",
            build: |ops, handlers| {
                let [opts, body]: [Ast; 2] = ops.try_into().expect("arity validated");
                Self::Within {
                    opts: Box::new(opts),
                    handlers,
                    body: Box::new(body),
                }
            },
        },
        ScopeKeyword {
            name: "grant",
            operands: &[Operand::Options { arms: false }, Operand::Atom],
            operand_desc: "capabilities, body",
            build: |ops, _| {
                let [caps, body]: [Ast; 2] = ops.try_into().expect("arity validated");
                Self::Grant {
                    caps: Box::new(caps),
                    body: Box::new(body),
                }
            },
        },
        ScopeKeyword {
            name: "audit",
            operands: &[Operand::Atom],
            operand_desc: "body",
            build: |ops, _| {
                let [body]: [Ast; 1] = ops.try_into().expect("arity validated");
                Self::Audit {
                    body: Box::new(body),
                }
            },
        },
    ];

    /// Look up a control-operator keyword by surface name.
    pub(crate) fn lookup_keyword(name: &str) -> Option<&'static ScopeKeyword> {
        Self::KEYWORDS.iter().find(|kw| kw.name == name)
    }
}

// ── Utilities ────────────────────────────────────────────────────────────

/// The value-literal shape of a bare word: the one answer to "literal or
/// command name?", read by the parser to skip the [`Ast::Call`] wrapper and
/// by elaboration through [`crate::ir::Val::from_word`].
///
/// Purely lexical: the numeral grammar decides, never a round trip through
/// printing.  A float wants a `.`, so `1e5`, which merely happens to f64-parse,
/// stays a string; `007` and `1.50` are numerals all the same, and normalise
/// to `7` and `1.5` wherever they are printed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WordLiteral {
    Bool(bool),
    Int(i64),
    Float(f64),
}

impl WordLiteral {
    pub(crate) fn classify(s: &str) -> Option<Self> {
        match s {
            "true" => Some(Self::Bool(true)),
            "false" => Some(Self::Bool(false)),
            _ => {
                if let Ok(i) = s.parse::<i64>() {
                    Some(Self::Int(i))
                } else if s.contains('.') {
                    // A Float is finite by construction; an overflowing
                    // literal like 1.0e999 stays a plain word.
                    s.parse()
                        .ok()
                        .filter(|f: &f64| f.is_finite())
                        .map(Self::Float)
                } else {
                    None
                }
            }
        }
    }
}

impl Pattern {
    /// The first name this pattern binds twice, if any. A pattern binds all
    /// its names at once, so a repeat is an ambiguity, not a shadow — the
    /// parser rejects it at both binder sites (`let` and lambda parameter).
    pub(crate) fn duplicate_name(&self) -> Option<&str> {
        fn walk<'a>(pat: &'a Pattern, seen: &mut HashSet<&'a str>) -> Option<&'a str> {
            match pat {
                Pattern::Wildcard => None,
                Pattern::Name(n) => (!seen.insert(n.as_str())).then_some(n.as_str()),
                Pattern::List { elems, rest } => elems
                    .iter()
                    .find_map(|e| walk(e, seen))
                    .or_else(|| rest.as_deref().filter(|r| !seen.insert(*r))),
                Pattern::Map(entries) => entries.iter().find_map(|e| walk(&e.pattern, seen)),
            }
        }
        walk(self, &mut HashSet::new())
    }

    pub(crate) fn collect_names(&self, set: &mut HashSet<String>) {
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
    pub(crate) fn is_thunk_form(&self) -> bool {
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
    /// Every sub-expression in source order: the operands of
    /// [`Self::KEYWORDS`], plus `within`'s handler arms, which are syntax and
    /// so sit beside the operands rather than inside one.  Free-variable
    /// collection walks them.
    pub(crate) fn operands(&self) -> Vec<&Ast> {
        match self {
            Self::Try { body, handler } => vec![body, handler],
            Self::Guard { body, cleanup } => vec![body, cleanup],
            Self::Within {
                opts,
                handlers,
                body,
            } => {
                let mut ops = vec![opts.as_ref()];
                ops.extend(handlers.iter().flatten().map(|arm| &arm.value.item));
                ops.push(body);
                ops
            }
            Self::Grant { caps, body } => vec![caps, body],
            Self::Audit { body } => vec![body],
        }
    }
}
