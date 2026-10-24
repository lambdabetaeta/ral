//! The scoping of a statement list (SPEC §5.7): its stretches of block
//! definitions, and the components that split a stretch into recursive groups.

use crate::ir::{Name, Pattern};
use crate::source::{Span, Spanned};
use crate::syntax::ast::{Ast, Stmt};
use std::collections::HashSet;

/// `let name = rhs`, `rhs` a block or a lambda: what a stretch admits.
pub(crate) struct Def<'a> {
    pub span: Option<Span>,
    pub name: Spanned<&'a Name>,
    pub value: &'a Spanned<Box<Ast>>,
}

impl<'a> Def<'a> {
    fn of(stmt: &'a Stmt) -> Option<Self> {
        let Ast::Let { pattern, value } = &stmt.item else {
            return None;
        };
        let Pattern::Name(name) = &pattern.item else {
            return None;
        };
        matches!(*value.item, Ast::Block(_) | Ast::Lambda { .. }).then(|| Self {
            span: stmt.span,
            name: Spanned::with_span(pattern.span, name),
            value,
        })
    }
}

/// A part of a statement list: its bindings scope over the parts after it,
/// and a stretch's also over the stretch itself.
pub(crate) enum Part<'a> {
    Stmt(&'a Stmt),
    Stretch(Vec<Def<'a>>),
}

impl<'a> Part<'a> {
    /// A stretch's definitions; a statement has none.
    pub(crate) fn defs(&self) -> &[Def<'a>] {
        match self {
            Self::Stmt(_) => &[],
            Self::Stretch(defs) => defs,
        }
    }
}

/// `stmts` as parts, in source order.
pub(crate) fn parts(stmts: &[Stmt]) -> Vec<Part<'_>> {
    let mut bound: HashSet<&Name> = HashSet::new();
    let mut out: Vec<Part<'_>> = Vec::new();
    for stmt in stmts {
        let Some(def) = Def::of(stmt) else {
            if let Ast::Let { pattern, .. } = &stmt.item {
                bound.extend(pattern.item.names());
            }
            out.push(Part::Stmt(stmt));
            continue;
        };
        let fresh = bound.insert(def.name.item);
        match out.last_mut() {
            Some(Part::Stretch(defs)) if fresh => defs.push(def),
            _ => out.push(Part::Stretch(vec![def])),
        }
    }
    out
}

/// Tarjan's strongly connected components of `edges` (`edges[i]`: the nodes
/// `i` refers to), dependencies first, each ascending.  Iterative: a long
/// chain of definitions would overflow the host stack.
pub(crate) fn sccs(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut tarjan = Tarjan {
        next: 0,
        index: vec![None; edges.len()],
        low: vec![0; edges.len()],
        on_stack: vec![false; edges.len()],
        stack: Vec::new(),
        out: Vec::new(),
    };
    for v in 0..edges.len() {
        if tarjan.index[v].is_none() {
            tarjan.connect(v, edges);
        }
    }
    tarjan.out
}

struct Tarjan {
    next: usize,
    index: Vec<Option<usize>>,
    low: Vec<usize>,
    on_stack: Vec<bool>,
    stack: Vec<usize>,
    out: Vec<Vec<usize>>,
}

impl Tarjan {
    fn discover(&mut self, v: usize) {
        self.index[v] = Some(self.next);
        self.low[v] = self.next;
        self.next += 1;
        self.stack.push(v);
        self.on_stack[v] = true;
    }

    /// Popping a frame folds its lowlink into its parent's: the work the
    /// recursive form does on return.
    fn connect(&mut self, v: usize, edges: &[Vec<usize>]) {
        self.discover(v);
        // Frame: a node, and where in its edges to resume.
        let mut work: Vec<(usize, usize)> = vec![(v, 0)];
        while let Some(&(node, at)) = work.last() {
            if let Some(&w) = edges[node].get(at) {
                work.last_mut().expect("non-empty").1 += 1;
                match self.index[w] {
                    None => {
                        self.discover(w);
                        work.push((w, 0));
                    }
                    Some(iw) if self.on_stack[w] => self.low[node] = self.low[node].min(iw),
                    Some(_) => {}
                }
                continue;
            }
            // A node whose lowlink never fell below its own index roots a
            // component: itself and everything above it on the stack.
            if self.index[node] == Some(self.low[node]) {
                let from = self
                    .stack
                    .iter()
                    .rposition(|&w| w == node)
                    .expect("on the stack");
                let mut component = self.stack.split_off(from);
                for &w in &component {
                    self.on_stack[w] = false;
                }
                component.sort_unstable();
                self.out.push(component);
            }
            work.pop();
            if let Some(&(parent, _)) = work.last() {
                self.low[parent] = self.low[parent].min(self.low[node]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::parser::parse;

    /// One descriptor per part: `stmt`, or `stretch [a, b]` naming its members.
    fn parts_of(src: &str) -> Vec<String> {
        let stmts = parse(src).expect("parse");
        parts(&stmts)
            .iter()
            .map(|part| match part {
                Part::Stmt(_) => "stmt".to_string(),
                Part::Stretch(defs) => {
                    let names: Vec<&str> = defs.iter().map(|def| def.name.item.as_ref()).collect();
                    format!("stretch [{}]", names.join(", "))
                }
            })
            .collect()
    }

    #[test]
    fn consecutive_block_definitions_form_one_stretch() {
        assert_eq!(
            parts_of("let f = { |x| g $x }\nlet g = { |y| f $y }\nreturn ()"),
            ["stretch [f, g]", "stmt"]
        );
    }

    /// `let x` ends the stretch, so `b` is not lifted past it.
    #[test]
    fn any_other_statement_ends_a_stretch() {
        assert_eq!(
            parts_of("let a = { b }\nlet x = 1\nlet b = { return $x }\necho !{a}"),
            ["stretch [a]", "stmt", "stretch [b]", "stmt"]
        );
    }

    /// Rebinding `f` begins a new stretch, so `g` keeps the `f` above it.
    #[test]
    fn a_rebound_name_begins_the_next_stretch() {
        assert_eq!(
            parts_of("let f = 10\nlet g = { return $f }\nlet f = { return 20 }"),
            ["stmt", "stretch [g]", "stretch [f]"]
        );
        assert_eq!(
            parts_of("let f = { 1 }\nlet g = { f }\nlet f = { 2 }\nlet h = { f }"),
            ["stretch [f, g]", "stretch [f, h]"]
        );
    }

    #[test]
    fn only_a_name_bound_to_a_block_or_lambda_joins_a_stretch() {
        assert_eq!(
            parts_of("let n = 5\nlet [a, b] = [1, 2]\nlet _ = { 1 }\nlet m = !{ 1 }"),
            ["stmt", "stmt", "stmt", "stmt"]
        );
    }

    /// 0 → 1 → 2 ⇄ 3, and 4 alone.
    #[test]
    fn components_come_dependencies_first() {
        let edges = [vec![1], vec![2], vec![3], vec![2], vec![]];
        assert_eq!(sccs(&edges), [vec![2, 3], vec![1], vec![0], vec![4]]);
    }

    /// The host-stack regression that makes the walk iterative.
    #[test]
    fn a_long_chain_does_not_overflow() {
        const N: usize = 50_000;
        let edges: Vec<Vec<usize>> = (0..=N)
            .map(|i| if i < N { vec![i + 1] } else { Vec::new() })
            .collect();
        let components = sccs(&edges);
        assert_eq!(components.len(), N + 1);
        assert_eq!(components[0], [N]);
    }
}
