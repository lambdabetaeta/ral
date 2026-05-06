//! Abstract syntax tree.
//!
//! The AST is produced by the parser and consumed by the elaborator. It is a
//! direct, untyped representation of the surface syntax: commands,
//! pipelines, blocks, lambdas, let-bindings, conditionals, and value
//! literals. Nodes carry no source spans; position markers ([`Ast::Pos`])
//! are interleaved into statement lists so the evaluator can update its
//! current-line indicator cheaply.
//!
//! The tree is serialisable (via `serde`) for debugging and the `to-json`
//! builtin.

use crate::span::Span;
use crate::path::tilde::TildePath;
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
    /// Variable binding: pattern = expr
    Let { pattern: Pattern, value: Box<Ast> },
    /// Explicit value-to-command lift: return <value>?
    Return(Option<Box<Ast>>),
    /// Function/command application in command context.
    ///
    /// `span` covers the head and all arguments — the full extent of the
    /// surface command, used by the elaborator to stamp the resulting
    /// `Comp::App` so diagnostics underline the whole command, not just
    /// the head's first token.
    App {
        head: Head,
        args: Vec<Ast>,
        span: Span,
    },
    /// A pipeline: cmd1 | cmd2 | cmd3
    Pipeline(Vec<Ast>),
    /// Chained commands: cmd1 ? cmd2 ? cmd3
    Chain(Vec<Ast>),
    /// Background execution: command &
    Background(Box<Ast>),
    /// A block: { ... }
    Block(Vec<Ast>),
    /// A lambda: { |params| body }
    Lambda { param: Param, body: Vec<Ast> },
    /// A list literal: [a, b, c]
    List(Vec<ListElem>),
    /// A map literal: [key: val, key: val]
    Map(Vec<MapEntry>),
    /// String interpolation: "hello $name"
    Interpolation(Vec<Ast>),
    /// Expression block: $[expr]
    Expr(Box<Expr>),
    /// Indexing: $name[k1][k2]
    Index { target: Box<Ast>, keys: Vec<Ast> },
    /// Force: ! atom
    Force(Box<Ast>),
    /// I/O redirect
    Redirect {
        fd: u32,
        mode: RedirectMode,
        target: RedirectTarget,
    },
    /// Conditional: `if cond then [elsif cond then]* [else else_]`.
    /// One-armed form (no else) has type Unit; multi-armed form requires
    /// both branches to agree on type.
    If {
        cond: Box<Ast>,
        then: Box<Ast>,
        elsif: Vec<(Ast, Ast)>,
        else_: Option<Box<Ast>>,
    },
    /// Source position marker (transparent — evaluator updates loc.line).
    Pos(Span),
}

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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Pattern {
    /// `_` -- discard the value.
    Wildcard,
    /// Bind the value to a name.
    Name(String),
    /// `[a, b, ...rest]` -- destructure a list. The optional `rest`
    /// captures the tail as a new list.
    List {
        elems: Vec<Pattern>,
        rest: Option<String>,
    },
    /// `[key: pat = default, ...]` -- destructure a map. Each entry is
    /// `(key_name, sub_pattern, optional_default)`.
    Map(Vec<(String, Pattern, Option<Ast>)>),
}

/// Lambda parameter. Always a single pattern; multi-parameter lambdas
/// are desugared by the parser into nested single-parameter lambdas
/// (currying).
pub type Param = Pattern;

/// Element of a list literal. A spread (`...expr`) splices another list
/// into the enclosing one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ListElem {
    /// An ordinary element.
    Single(Ast),
    /// `...expr` -- splice the elements of `expr` into this list.
    Spread(Ast),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MapEntry {
    Entry(Ast, Ast),
    Spread(Ast),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    Integer(i64),
    Number(f64),
    Bool(bool),
    Var(String),
    Index(String, Vec<Ast>),
    Force(Box<Ast>),
    BinOp(Box<Expr>, ExprOp, Box<Expr>),
    /// Unary logical negation: `not e` (strict).
    Not(Box<Expr>),
    /// Short-circuit conjunction: `a && b`.  RHS is evaluated only if
    /// LHS is `true`.  Desugars in the elaborator to `_if a { b } { return false }`.
    And(Box<Expr>, Box<Expr>),
    /// Short-circuit disjunction: `a || b`.  RHS is evaluated only if
    /// LHS is `false`.
    Or(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ExprOp {
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
    /// Unary logical not.  The only unary variant in `ExprOp`; the
    /// evaluator dispatches on arity via `Comp::PrimOp`'s `Vec<Val>`.
    Not,
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

// ── Utilities ────────────────────────────────────────────────────────────

pub fn is_value_literal_name(s: &str) -> bool {
    matches!(s, "true" | "false" | "unit")
        || s.parse::<i64>().is_ok()
        || (s.contains('.') && s.parse::<f64>().is_ok())
}

impl Pattern {
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
                for (_, p, _) in entries {
                    p.collect_names(set);
                }
            }
        }
    }
}

/// Record `n` in `out` if it is a candidate and not shadowed by an enclosing
/// lambda scope.  The same predicate fires from three traversals (Ast, Expr,
/// Head); factoring keeps shadowing logic in one place.
fn note_free(
    n: &str,
    candidates: &HashSet<String>,
    scopes: &[HashSet<String>],
    out: &mut HashSet<String>,
) {
    if candidates.contains(n) && !scopes.iter().any(|s| s.contains(n)) {
        out.insert(n.to_string());
    }
}

impl Ast {
    pub fn is_lambda(&self) -> bool {
        matches!(self, Ast::Lambda { .. } | Ast::Block(_))
    }

    /// Collect free references to names in `candidates`, respecting lambda scopes.
    pub fn free_refs(&self, candidates: &HashSet<String>) -> HashSet<String> {
        let mut out = HashSet::new();
        let mut scopes: Vec<HashSet<String>> = Vec::new();
        self.collect_free_refs(candidates, &mut scopes, &mut out);
        out
    }

    fn collect_free_refs(
        &self,
        candidates: &HashSet<String>,
        scopes: &mut Vec<HashSet<String>>,
        out: &mut HashSet<String>,
    ) {
        match self {
            Ast::Variable(n) => note_free(n, candidates, scopes, out),
            Ast::Literal(_)
            | Ast::Word(Word::Plain(_))
            | Ast::Word(Word::Slash(_))
            | Ast::Word(Word::Tilde(_))
            | Ast::Pos(_)
            | Ast::Return(None) => {}
            Ast::Lambda { param, body } => {
                let mut names = HashSet::new();
                param.collect_names(&mut names);
                scopes.push(names);
                for ast in body {
                    ast.collect_free_refs(candidates, scopes, out);
                }
                scopes.pop();
            }
            Ast::Block(stmts) => {
                for ast in stmts {
                    ast.collect_free_refs(candidates, scopes, out);
                }
            }
            Ast::Let { value, .. } | Ast::Return(Some(value)) => {
                value.collect_free_refs(candidates, scopes, out);
            }
            Ast::App { head, args, .. } => {
                head.collect_free_refs(candidates, scopes, out);
                for arg in args {
                    arg.collect_free_refs(candidates, scopes, out);
                }
            }
            Ast::Pipeline(stages) | Ast::Chain(stages) | Ast::Interpolation(stages) => {
                for s in stages {
                    s.collect_free_refs(candidates, scopes, out);
                }
            }
            Ast::Background(inner) | Ast::Force(inner) => {
                inner.collect_free_refs(candidates, scopes, out);
            }
            Ast::Expr(expr) => {
                expr.collect_free_refs(candidates, scopes, out);
            }
            Ast::Index { target, keys } => {
                target.collect_free_refs(candidates, scopes, out);
                for k in keys {
                    k.collect_free_refs(candidates, scopes, out);
                }
            }
            Ast::List(elems) => {
                for elem in elems {
                    match elem {
                        ListElem::Single(a) | ListElem::Spread(a) => {
                            a.collect_free_refs(candidates, scopes, out);
                        }
                    }
                }
            }
            Ast::Map(entries) => {
                for entry in entries {
                    match entry {
                        MapEntry::Entry(k, v) => {
                            k.collect_free_refs(candidates, scopes, out);
                            v.collect_free_refs(candidates, scopes, out);
                        }
                        MapEntry::Spread(a) => {
                            a.collect_free_refs(candidates, scopes, out);
                        }
                    }
                }
            }
            Ast::Redirect { target, .. } => {
                if let RedirectTarget::File(ast) = target {
                    ast.collect_free_refs(candidates, scopes, out);
                }
            }
            Ast::If {
                cond,
                then,
                elsif,
                else_,
            } => {
                cond.collect_free_refs(candidates, scopes, out);
                then.collect_free_refs(candidates, scopes, out);
                for (ec, et) in elsif {
                    ec.collect_free_refs(candidates, scopes, out);
                    et.collect_free_refs(candidates, scopes, out);
                }
                if let Some(e) = else_ {
                    e.collect_free_refs(candidates, scopes, out);
                }
            }
        }
    }
}

impl Expr {
    fn collect_free_refs(
        &self,
        candidates: &HashSet<String>,
        scopes: &mut Vec<HashSet<String>>,
        out: &mut HashSet<String>,
    ) {
        match self {
            Expr::Integer(_) | Expr::Number(_) | Expr::Bool(_) => {}
            Expr::Var(n) => note_free(n, candidates, scopes, out),
            Expr::Index(n, keys) => {
                note_free(n, candidates, scopes, out);
                for k in keys {
                    k.collect_free_refs(candidates, scopes, out);
                }
            }
            Expr::Force(inner) => {
                inner.collect_free_refs(candidates, scopes, out);
            }
            Expr::BinOp(l, _, r) | Expr::And(l, r) | Expr::Or(l, r) => {
                l.collect_free_refs(candidates, scopes, out);
                r.collect_free_refs(candidates, scopes, out);
            }
            Expr::Not(inner) => {
                inner.collect_free_refs(candidates, scopes, out);
            }
        }
    }
}

impl Head {
    fn collect_free_refs(
        &self,
        candidates: &HashSet<String>,
        scopes: &mut Vec<HashSet<String>>,
        out: &mut HashSet<String>,
    ) {
        match self {
            Head::Bare(n) => note_free(n, candidates, scopes, out),
            Head::Value(ast) => ast.collect_free_refs(candidates, scopes, out),
            Head::ExternalName(_) | Head::Path(_) | Head::TildePath(_) => {}
        }
    }
}
