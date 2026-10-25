use super::{
    Arc, Ast, Comp, CompKind, Def, Elaborator, GroupNode, Name, Pattern, Phrase, Span, Spanned,
    Stmt, StretchGraph, Val, WithSpan, group, wrap_binds,
};

impl Elaborator {
    /// A non-`let` statement's own `Comp`: the hoisting boundary, folding
    /// whatever the subtree pushed into `binds` into a `Comp::Bind` chain.
    fn stmt(&mut self, ast: &Ast) -> Comp {
        let mut binds = Vec::new();
        let comp = self.elab_expr(ast, &mut binds);
        wrap_binds(self.current_span, binds, comp)
    }

    /// A `let pattern = value`'s right-hand side, under its own span so a
    /// bind failure in `let [a, b] = 42` underlines `42`, not the statement.
    /// The pattern's names are not yet in scope: the caller binds them once
    /// it decides where `let x = x` should read the outer `x` from.
    fn elab_let_rhs(&mut self, value: &Spanned<Box<Ast>>) -> Comp {
        let mut binds = Vec::new();
        let comp = self.with_span(value.span, |this| this.elab_expr(&value.item, &mut binds));
        // The temporaries wrap the right-hand side, not the `Bind`: only the
        // RHS reads them, and a frame around the `Bind` would take the
        // user's own binding down with them.
        wrap_binds(self.current_span, binds, comp)
    }

    /// A stretch (SPEC §5.7) as bindings: all its names are bound before any
    /// member elaborates; components come dependencies first, a cyclic one
    /// as one `Rec` group, Levy's `rec x⃗. M⃗`.
    pub(super) fn elab_stretch(
        &mut self,
        defs: &[Def<'_>],
        after: &[group::Part<'_>],
    ) -> Vec<(Option<Span>, Name, Comp)> {
        for def in defs {
            self.bind(def.name.span, [def.name.item.clone()]);
        }
        self.stretches.push(StretchGraph {
            frame: self.lexical_scopes.len() - 1,
            members: defs
                .iter()
                .enumerate()
                .map(|(i, def)| (def.name.item.clone(), i))
                .collect(),
            current: 0,
            edges: vec![Vec::new(); defs.len()],
            later: after
                .iter()
                .flat_map(group::Part::defs)
                .map(|def| def.name.item.clone())
                .collect(),
        });
        let mut rhs: Vec<Option<Comp>> = Vec::with_capacity(defs.len());
        for (i, def) in defs.iter().enumerate() {
            self.stretches.last_mut().expect("pushed above").current = i;
            rhs.push(Some(
                self.with_span(def.span, |this| this.elab_let_rhs(def.value)),
            ));
        }
        let mut edges = self.stretches.pop().expect("pushed above").edges;
        for refs in &mut edges {
            refs.sort_unstable();
            refs.dedup();
        }
        let name = |i: usize| -> Name { defs[i].name.item.clone() };
        let mut bindings = Vec::with_capacity(defs.len());
        for component in group::sccs(&edges) {
            let mut take = |i: usize| rhs[i].take().expect("each member is in one component");
            if let [i] = component[..]
                && !edges[i].contains(&i)
            {
                bindings.push((defs[i].span, name(i), take(i)));
                continue;
            }
            let group = GroupNode::new(
                component
                    .iter()
                    .map(|&i| {
                        let CompKind::Return(Val::Thunk(node)) = take(i).item else {
                            unreachable!("a block or lambda elaborates to Return(Thunk(_))")
                        };
                        (name(i), Arc::clone(node.shape()))
                    })
                    .collect(),
            );
            bindings.extend(component.iter().enumerate().map(|(index, &i)| {
                let span = defs[i].value.span;
                let rec = Spanned::with_span(
                    span,
                    CompKind::Rec {
                        group: Arc::clone(&group),
                        index,
                    },
                );
                let comp = Spanned::with_span(span, CompKind::Return(Val::thunk(Arc::new(rec))));
                (span, name(i), comp)
            }));
        }
        bindings
    }

    /// One top-level phrase (depth 0): a `let` becomes a `Define`,
    /// anything else a `Run`.  The `Define` pattern's names enter scope only
    /// after the RHS is elaborated, as `nested_single`'s `let` arm does.
    pub(super) fn toplevel_phrase(&mut self, stmt: &Stmt) -> Spanned<Phrase<()>> {
        let Spanned { item: kind, span } = stmt;
        let span = *span;
        self.with_span(span, |this| {
            if let Ast::Let { pattern, value } = kind {
                let rhs = this.elab_let_rhs(value);
                this.bind_pattern(pattern);
                return Spanned::with_span(
                    span,
                    Phrase::Define {
                        pattern: Arc::new(pattern.item.clone()),
                        comp: Arc::new(rhs),
                        schemes: (),
                    },
                );
            }
            let mut binds = Vec::new();
            let comp = this.elab_expr(kind, &mut binds);
            let comp = wrap_binds(this.current_span, binds, comp);
            Spanned::with_span(span, Phrase::Run(Arc::new(comp)))
        })
    }

    /// Elaborate `stmts` at depth > 0 — every block or lambda body — right-
    /// nesting each statement into a `Bind` chain over what follows it, so
    /// `{ a; b }` is `a to _. b`.  The tail of the block is its last
    /// statement's own comp, not a `Bind` on it; a block ending in a `let`
    /// has tail `Return(Unit)`.
    pub(super) fn stmts_nested(&mut self, stmts: &[Stmt]) -> Comp {
        let mut units: Vec<(Option<Span>, NestedUnit)> = Vec::new();
        let parts = group::parts(stmts);
        for (k, part) in parts.iter().enumerate() {
            match part {
                group::Part::Stmt(stmt) => units.push(self.nested_single(stmt)),
                group::Part::Stretch(defs) => {
                    let bindings = self.elab_stretch(defs, &parts[k + 1..]);
                    units.extend(bindings.into_iter().map(|(span, name, rhs)| {
                        let pattern = Pattern::Name(name);
                        (span, NestedUnit::Bind { rhs, pattern })
                    }));
                }
            }
        }
        let mut rev = units.into_iter().rev();
        let Some((span, last)) = rev.next() else {
            return comp!(self, CompKind::Return(Val::Unit));
        };
        let mut rest = nested_tail(span, last);
        for (span, unit) in rev {
            rest = unit.into_bind(span, rest);
        }
        rest
    }

    /// One statement at depth > 0: a `let` binds a fresh `Name` (never
    /// `Wildcard` — a surface `_` gets the hygienic gensym `$[…]` temporaries
    /// already use), and anything else is a discard, marked `Wildcard`,
    /// which no surface pattern ever produces.
    fn nested_single(&mut self, stmt: &Stmt) -> (Option<Span>, NestedUnit) {
        let Spanned { item: kind, span } = stmt;
        let span = *span;
        self.with_span(span, |this| {
            if let Ast::Let { pattern, value } = kind {
                let rhs = this.elab_let_rhs(value);
                let pattern_ir = match &pattern.item {
                    Pattern::Wildcard => Pattern::Name(this.gensym()),
                    named => named.clone(),
                };
                this.bind_pattern(pattern);
                return (
                    span,
                    NestedUnit::Bind {
                        rhs,
                        pattern: pattern_ir,
                    },
                );
            }
            let comp = this.stmt(kind);
            (span, NestedUnit::Other { comp })
        })
    }
}

/// One statement of a block, elaborated but not yet nested over what
/// follows it — [`Elaborator::stmts_nested`]'s intermediate form.
enum NestedUnit {
    /// A `let` or a recursive group's member: `rest` is the elaboration of
    /// what follows.
    Bind { rhs: Comp, pattern: Pattern },
    /// Any other statement, discarded on a `Wildcard` bind.
    Other { comp: Comp },
}

impl NestedUnit {
    /// Nest over `rest`: a `let` binds its pattern, anything else is discarded.
    fn into_bind(self, span: Option<Span>, rest: Comp) -> Comp {
        let (pattern, comp) = match self {
            Self::Bind { rhs, pattern } => (pattern, rhs),
            Self::Other { comp } => (Pattern::Wildcard, comp),
        };
        Spanned::with_span(span, CompKind::bind(pattern, comp, rest))
    }
}

/// The last unit of a block: a `let` still needs a `rest` — the block's own
/// value, `Unit` — but a plain statement's comp *is* the tail.
fn nested_tail(span: Option<Span>, unit: NestedUnit) -> Comp {
    match unit {
        NestedUnit::Other { comp } => comp,
        bind @ NestedUnit::Bind { .. } => {
            bind.into_bind(span, Spanned::with_span(span, CompKind::Return(Val::Unit)))
        }
    }
}
