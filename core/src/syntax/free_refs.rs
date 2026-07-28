//! Free-variable analysis over the AST: which of a set of candidate names does
//! an expression reference without binding them itself?
//!
//! `group` builds its dependency graph over `let`-bound lambdas from this, and
//! the strongly connected components of that graph become the `LetRec` knots.

use crate::syntax::ast::{
    Ast, Expr, Head, ListElem, MapEntry, Pattern, Redirect, RedirectTarget, ScopeAst, Stmt, Word,
};
use std::collections::HashSet;

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

/// Walk a block or lambda body.  Each `let`'s own RHS is visited before its
/// names are pushed, so a `let` never binds itself, only the statements after
/// it; `scopes` is restored on return.
fn collect_stmts_free_refs(
    stmts: &[Stmt],
    candidates: &HashSet<String>,
    scopes: &mut Vec<HashSet<String>>,
    out: &mut HashSet<String>,
) {
    let mut pushed = 0;
    for stmt in stmts {
        stmt.item.collect_free_refs(candidates, scopes, out);
        if let Ast::Let { pattern, .. } = &stmt.item {
            let mut names = HashSet::new();
            pattern.item.collect_names(&mut names);
            scopes.push(names);
            pushed += 1;
        }
    }
    for _ in 0..pushed {
        scopes.pop();
    }
}

impl Ast {
    /// The names in `candidates` this AST references without binding.
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
            Self::Variable(n) => note_free(n, candidates, scopes, out),
            Self::Literal(_)
            | Self::Word(Word::Plain(_) | Word::Slash(_) | Word::Tilde(_))
            | Self::Return(None) => {}
            Self::Lambda { param, body } => {
                param
                    .item
                    .collect_default_free_refs(candidates, scopes, out);
                let mut names = HashSet::new();
                param.item.collect_names(&mut names);
                scopes.push(names);
                collect_stmts_free_refs(body, candidates, scopes, out);
                scopes.pop();
            }
            Self::Block(stmts) => {
                collect_stmts_free_refs(stmts, candidates, scopes, out);
            }
            Self::Let { pattern, value } => {
                pattern
                    .item
                    .collect_default_free_refs(candidates, scopes, out);
                value.item.collect_free_refs(candidates, scopes, out);
            }
            Self::Return(Some(value))
            | Self::Background(value)
            | Self::Spread(value)
            | Self::Force(value) => {
                value.item.collect_free_refs(candidates, scopes, out);
            }
            Self::Call {
                head,
                args,
                redirects,
            } => {
                head.collect_free_refs(candidates, scopes, out);
                for arg in args {
                    arg.item.collect_free_refs(candidates, scopes, out);
                }
                for r in redirects {
                    r.collect_free_refs(candidates, scopes, out);
                }
            }
            Self::Scope { op, redirects } => {
                op.collect_free_refs(candidates, scopes, out);
                for r in redirects {
                    r.collect_free_refs(candidates, scopes, out);
                }
            }
            Self::Pipeline(stages) | Self::Chain(stages) | Self::Interpolation(stages) => {
                for s in stages {
                    s.item.collect_free_refs(candidates, scopes, out);
                }
            }
            Self::Tag { payload, .. } => {
                if let Some(p) = payload {
                    p.item.collect_free_refs(candidates, scopes, out);
                }
            }
            Self::Case { scrutinee, table } => {
                scrutinee.item.collect_free_refs(candidates, scopes, out);
                table.item.collect_free_refs(candidates, scopes, out);
            }
            Self::Expr(expr) => {
                expr.collect_free_refs(candidates, scopes, out);
            }
            Self::Index { target, keys } => {
                target.item.collect_free_refs(candidates, scopes, out);
                for k in keys {
                    k.item.collect_free_refs(candidates, scopes, out);
                }
            }
            Self::List(elems) => {
                for elem in elems {
                    match elem {
                        ListElem::Single(a) | ListElem::Spread(a) => {
                            a.item.collect_free_refs(candidates, scopes, out);
                        }
                    }
                }
            }
            Self::Map(entries) => {
                for entry in entries {
                    match entry {
                        MapEntry::Entry { value, .. } => {
                            value.item.collect_free_refs(candidates, scopes, out);
                        }
                        MapEntry::Deref { name, value } => {
                            note_free(name, candidates, scopes, out);
                            value.item.collect_free_refs(candidates, scopes, out);
                        }
                        MapEntry::Spread(a) => {
                            a.item.collect_free_refs(candidates, scopes, out);
                        }
                    }
                }
            }
            Self::If { branches, else_ } => {
                for branch in branches {
                    branch.cond.item.collect_free_refs(candidates, scopes, out);
                    branch.body.item.collect_free_refs(candidates, scopes, out);
                }
                if let Some(e) = else_ {
                    e.item.collect_free_refs(candidates, scopes, out);
                }
            }
        }
    }
}

impl Pattern {
    /// Free references in this pattern's map-entry defaults.  A default is
    /// evaluated before the pattern's own names bind, so it sees only the
    /// enclosing scopes — hence the unextended `scopes` the caller passes.
    fn collect_default_free_refs(
        &self,
        candidates: &HashSet<String>,
        scopes: &mut Vec<HashSet<String>>,
        out: &mut HashSet<String>,
    ) {
        match self {
            Self::Wildcard | Self::Name(_) => {}
            Self::List { elems, .. } => {
                for e in elems {
                    e.collect_default_free_refs(candidates, scopes, out);
                }
            }
            Self::Map(entries) => {
                for entry in entries {
                    if let Some(default) = &entry.default {
                        default.collect_free_refs(candidates, scopes, out);
                    }
                    entry
                        .pattern
                        .collect_default_free_refs(candidates, scopes, out);
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
            Self::Integer(_) | Self::Number(_) | Self::Bool(_) => {}
            Self::Variable(n) => note_free(n, candidates, scopes, out),
            Self::Index(n, keys) => {
                note_free(n, candidates, scopes, out);
                for k in keys {
                    k.item.collect_free_refs(candidates, scopes, out);
                }
            }
            Self::Force(inner) => {
                inner.item.collect_free_refs(candidates, scopes, out);
            }
            Self::BinOp(l, _, r) | Self::And(l, r) | Self::Or(l, r) => {
                l.collect_free_refs(candidates, scopes, out);
                r.collect_free_refs(candidates, scopes, out);
            }
            Self::Not(inner) => {
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
            // A bare head resolves through value lookup before PATH, so `f 1 2`
            // may well be calling a `let`-bound lambda.
            Self::Bare(n) => note_free(n, candidates, scopes, out),
            Self::Value(ast) => ast.collect_free_refs(candidates, scopes, out),
            Self::ExternalName(_) | Self::Path(_) | Self::TildePath(_) => {}
        }
    }
}

impl Redirect {
    fn collect_free_refs(
        &self,
        candidates: &HashSet<String>,
        scopes: &mut Vec<HashSet<String>>,
        out: &mut HashSet<String>,
    ) {
        if let RedirectTarget::File(ast) = &self.target {
            ast.collect_free_refs(candidates, scopes, out);
        }
    }
}

impl ScopeAst {
    fn collect_free_refs(
        &self,
        candidates: &HashSet<String>,
        scopes: &mut Vec<HashSet<String>>,
        out: &mut HashSet<String>,
    ) {
        for op in self.operands() {
            op.collect_free_refs(candidates, scopes, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::syntax::ast::Ast;
    use crate::syntax::parser::parse;
    use std::collections::HashSet;

    fn candidates(names: &[&str]) -> HashSet<String> {
        names.iter().map(std::string::ToString::to_string).collect()
    }

    /// Free references of `rhs_src` among `cands`, sorted for stable assertions.
    fn refs_of(rhs_src: &str, cands: &[&str]) -> Vec<String> {
        let src = format!("let _probe = {rhs_src}");
        let stmts = parse(&src).expect("parse");
        let Ast::Let { value, .. } = &stmts[0].item else {
            panic!("expected a let binding");
        };
        let mut out: Vec<String> = value
            .item
            .free_refs(&candidates(cands))
            .into_iter()
            .collect();
        out.sort_unstable();
        out
    }

    #[test]
    fn variable_reference_is_free() {
        assert_eq!(refs_of("$x", &["x", "y"]), vec!["x"]);
    }

    #[test]
    fn only_candidates_are_reported() {
        assert_eq!(refs_of("$x", &["x"]), vec!["x"]);
        assert!(refs_of("$y", &["x"]).is_empty());
    }

    #[test]
    fn lambda_parameter_shadows_a_candidate() {
        assert!(refs_of("{ |x| $x }", &["x"]).is_empty());
        assert_eq!(refs_of("{ |x| $g }", &["g", "x"]), vec!["g"]);
    }

    #[test]
    fn references_inside_expression_block_are_seen() {
        assert_eq!(refs_of("{ return $[$n + 1] }", &["n"]), vec!["n"]);
    }

    #[test]
    fn references_through_collections_and_interpolation() {
        assert_eq!(refs_of("[$a, $b]", &["a", "b", "c"]), vec!["a", "b"]);
        assert_eq!(refs_of("\"x $a y\"", &["a"]), vec!["a"]);
    }

    #[test]
    fn let_binding_in_lambda_body_scopes_over_later_statements() {
        assert!(refs_of("{ |x| let y = 1\n $y }", &["y"]).is_empty());
        assert_eq!(refs_of("{ |x| $g\n let y = 1 }", &["g", "y"]), vec!["g"]);
    }

    #[test]
    fn nested_lambda_scopes_stack() {
        assert_eq!(
            refs_of("{ |x| { |y| g $x $y } }", &["g", "x", "y"]),
            vec!["g"]
        );
    }

    #[test]
    fn map_pattern_default_in_lambda_param_is_free() {
        // `$g` is evaluated before the parameter's own names bind, so it escapes
        // the lambda even though the parameter binds a name of its own.
        assert_eq!(refs_of("{ |[k: d = $g]| return $d }", &["g"]), vec!["g"]);
    }

    #[test]
    fn map_pattern_default_in_let_binding_is_free() {
        let stmts = parse("let [k: d = $g] = $m").expect("parse");
        let mut out: Vec<String> = stmts[0]
            .item
            .free_refs(&candidates(&["g", "m"]))
            .into_iter()
            .collect();
        out.sort_unstable();
        assert_eq!(out, vec!["g", "m"]);
    }

    #[test]
    fn command_head_and_args_are_scanned() {
        assert_eq!(refs_of("{ $f 1 2 }", &["f"]), vec!["f"]);
    }
}
