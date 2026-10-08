//! Free-variable analysis over the AST: which of a set of candidate names does
//! an expression reference without binding them itself?
//!
//! `group` builds its dependency graph over `let`-bound lambdas from this, and
//! the strongly connected components of that graph become the `LetRec` knots.

use crate::ir::Name;
use crate::syntax::ast::{Ast, Head, ListElem, MapEntry, RecordEntry, ScopeAst, Stmt, Word};
use std::collections::HashSet;

/// The walk's state: the names asked about, the binders in scope, and what has
/// been found free.
struct FreeRefs<'a> {
    candidates: &'a HashSet<String>,
    scopes: Vec<HashSet<Name>>,
    out: HashSet<String>,
}

impl FreeRefs<'_> {
    fn note(&mut self, n: &str) {
        if self.candidates.contains(n) && !self.scopes.iter().any(|s| s.contains(n)) {
            self.out.insert(n.to_string());
        }
    }

    /// Walk a block or lambda body.  Each `let`'s own RHS is visited before its
    /// names are pushed, so a `let` never binds itself, only the statements after
    /// it; `scopes` is restored on return.
    fn stmts(&mut self, stmts: &[Stmt]) {
        let mut pushed = 0;
        for stmt in stmts {
            self.ast(&stmt.item);
            if let Ast::Let { pattern, .. } = &stmt.item {
                self.scopes
                    .push(pattern.item.names().into_iter().cloned().collect());
                pushed += 1;
            }
        }
        for _ in 0..pushed {
            self.scopes.pop();
        }
    }

    fn ast(&mut self, ast: &Ast) {
        match ast {
            Ast::Variable(n) => self.note(n),
            Ast::Unit
            | Ast::Literal(_)
            | Ast::Word(Word::Plain(_) | Word::Slash(_) | Word::Tilde(_))
            | Ast::Return(None) => {}
            Ast::Lambda { param, body } => {
                self.scopes
                    .push(param.item.names().into_iter().cloned().collect());
                self.stmts(body);
                self.scopes.pop();
            }
            Ast::Block(stmts) => {
                self.stmts(stmts);
            }
            Ast::Let { value, .. }
            | Ast::Return(Some(value))
            | Ast::Spread(value)
            | Ast::Force(value) => {
                self.ast(&value.item);
            }
            Ast::Call { head, args } => {
                self.head(head);
                for arg in args {
                    self.ast(&arg.item);
                }
            }
            Ast::Redirected { stage, redirects } => {
                self.ast(&stage.item);
                for r in redirects.operands() {
                    self.ast(r);
                }
            }
            Ast::Scope(op) => self.scope(op),
            Ast::Pipeline(stages) | Ast::Chain(stages) | Ast::Interpolation(stages) => {
                for s in stages {
                    self.ast(&s.item);
                }
            }
            Ast::Tag { payload, .. } => {
                if let Some(p) = payload {
                    self.ast(&p.item);
                }
            }
            Ast::Case { scrutinee, arms } => {
                self.ast(&scrutinee.item);
                for arm in arms {
                    self.ast(&arm.body.item);
                }
            }
            Ast::Binary(l, _, r) | Ast::And(l, r) | Ast::Or(l, r) => {
                self.ast(&l.item);
                self.ast(&r.item);
            }
            Ast::Negate(inner) | Ast::Not(inner) => {
                self.ast(&inner.item);
            }
            Ast::Index { target, keys } => {
                self.ast(&target.item);
                for k in keys {
                    self.ast(&k.item);
                }
            }
            Ast::List(elems) => {
                for elem in elems {
                    match elem {
                        ListElem::Single(a) | ListElem::Spread(a) => {
                            self.ast(&a.item);
                        }
                    }
                }
            }
            Ast::Record(entries) => {
                for entry in entries {
                    match entry {
                        RecordEntry::Field { value, .. } => {
                            self.ast(&value.item);
                        }
                        RecordEntry::Spread(a) => {
                            self.ast(&a.item);
                        }
                    }
                }
            }
            Ast::Map(entries) => {
                for entry in entries {
                    match entry {
                        MapEntry::Entry { key, value } => {
                            self.ast(&key.item);
                            self.ast(&value.item);
                        }
                        MapEntry::Spread(a) => {
                            self.ast(&a.item);
                        }
                    }
                }
            }
            Ast::If { branches, else_ } => {
                for branch in branches {
                    self.ast(&branch.cond.item);
                    self.ast(&branch.body.item);
                }
                if let Some(e) = else_ {
                    self.ast(&e.item);
                }
            }
        }
    }

    fn head(&mut self, head: &Head) {
        match head {
            // A bare head resolves through value lookup before PATH, so `f 1 2`
            // may well be calling a `let`-bound lambda.
            Head::Bare(n) => self.note(n),
            Head::Value(ast) => self.ast(ast),
            Head::ExternalName(_) | Head::Path(_) | Head::TildePath(_) => {}
        }
    }

    fn scope(&mut self, scope: &ScopeAst) {
        for op in scope.operands() {
            self.ast(op);
        }
    }
}

impl Ast {
    /// The names in `candidates` this AST references without binding.
    pub fn free_refs(&self, candidates: &HashSet<String>) -> HashSet<String> {
        let mut walk = FreeRefs {
            candidates,
            scopes: Vec::new(),
            out: HashSet::new(),
        };
        walk.ast(self);
        walk.out
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
        assert_eq!(refs_of("$y", &["x"]), Vec::<String>::new());
    }

    #[test]
    fn lambda_parameter_shadows_a_candidate() {
        assert_eq!(refs_of("{ |x| $x }", &["x"]), Vec::<String>::new());
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
        assert_eq!(
            refs_of("{ |x| let y = 1\n $y }", &["y"]),
            Vec::<String>::new()
        );
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
    fn command_head_and_args_are_scanned() {
        assert_eq!(refs_of("{ $f 1 2 }", &["f"]), vec!["f"]);
    }
}
