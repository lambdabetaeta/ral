//! Elaboration: surface AST into the call-by-push-value IR of [`crate::ir`].
//!
//! CBPV splits inert `Val` from effectful `Comp`.  Where the IR wants a `Val`
//! but the source has an effectful sub-expression, the elaborator binds it to
//! a fresh `_gN` and substitutes a `Val::Variable`; those pending bindings ride
//! a mutable *binds* accumulator threaded through `elab_expr`, which
//! `wrap_binds` folds into a `Comp::Bind` chain at a statement boundary.
//! A context that may not run — an `if` arm, a `?` chain arm, a pipeline stage
//! — must therefore hand its subtree a fresh accumulator, or the untaken arm's
//! effects escape the guard.
//!
//! The other job is command dispatch.  A bare head that is lexically bound
//! becomes `Force(Variable)` applied to its arguments; an unbound one becomes
//! `Exec` against the command namespace.  Every other head shape (`^name`,
//! `./x`, `~/x`, `$f`, `{ … }`) declares which it is syntactically.

use crate::ir::{
    Args, ArmBody, CaseArm, CommandName, CommandWord, Comp, CompKind, Exec, IrPattern, PipeYield,
    Phrase, RedirectV, Register, Toplevel, Val, ValListElem, ValMapEntry, ValRedirectTarget,
};
use crate::prelude_manifest;
use crate::source::Span;
use crate::source::Spanned;
use crate::source::WithSpan;
use crate::syntax::ast::{
    self, Ast, Expr, Head, IfBranch, ListElem, MapEntry, MapPatternEntry, Pattern, Redirect,
    RedirectTarget, ScopeAst, Stmt, Word,
};
use crate::syntax::group::{StmtGroup, group_stmts};
use crate::syntax::parser::ParseError;
use std::collections::HashSet;
use std::sync::Arc;

/// State threaded through the elaboration pass.
struct Elaborator {
    /// Fresh-name counter for `gensym`.
    counter: usize,
    /// Prelude exports, in scope beneath every lexical frame.  Shared and never
    /// mutated, so an elaboration bumps a refcount rather than cloning the set.
    prelude: Arc<HashSet<String>>,
    /// Bound names, innermost last; the base frame holds the caller's bindings.
    lexical_scopes: Vec<HashSet<String>>,
    /// Attached to every emitted `Comp`; narrowed and restored by `with_span`,
    /// which every traversal that knows a tighter byte range wraps its body in.
    current_span: Option<Span>,
    /// This source's own display name, `None` where there is no self-location
    /// to bake (the REPL, `-c`, synthetic `<...>` sources).  What `$SCRIPT`
    /// resolves to.
    script: Option<String>,
    /// Elaboration's one failure path, checked by `elaborate` once the walk is
    /// done — a single slot beats threading `Result` through every traversal.
    error: Option<ParseError>,
}

/// Wrap a `CompKind` using the elaborator's current span.
macro_rules! comp {
    ($self:expr, $kind:expr) => {
        Spanned::with_span($self.current_span, $kind)
    };
}

impl WithSpan for Elaborator {
    fn span_slot(&mut self) -> &mut Option<Span> {
        &mut self.current_span
    }
}

impl Elaborator {
    /// `bindings` are names already live in the caller (REPL definitions, say);
    /// `name` is the source's own display name.
    fn new_with_bindings(bindings: HashSet<String>, name: &str) -> Self {
        Self {
            counter: 0,
            prelude: prelude_scope(),
            lexical_scopes: vec![bindings],
            current_span: None,
            script: crate::path::lex::has_script_identity(name).then(|| name.to_string()),
            error: None,
        }
    }

    /// Elaborate a `$name` reference.  `SCRIPT` bakes to a string literal
    /// rather than a runtime lookup: self-location is lexical (bash's
    /// `BASH_SOURCE`, not `$0`'s caller-site), and a compile-time literal is
    /// lexicality by construction.
    fn variable_val(&mut self, name: &str) -> Val {
        if name != "SCRIPT" {
            return Val::Variable(name.to_string());
        }
        if let Some(s) = &self.script {
            return Val::String(s.clone());
        }
        self.error.get_or_insert_with(|| ParseError {
            message: "$SCRIPT: no script name here (the REPL, `-c`, and preloaded \
                      sources have none)"
                .into(),
            span: self.current_span,
            lex_kind: None,
            incompleteness: None,
        });
        Val::Unit
    }

    /// A name for a hoisted temporary.  Skips anything already bound: `_` is
    /// ral's internal namespace, not an unwritable one, so a user's own `_var2`
    /// must not be capturable by a temporary that happens to land on it.
    fn gensym(&mut self) -> String {
        loop {
            self.counter += 1;
            let name = format!("_var{}", self.counter);
            if !self.is_bound(&name) {
                return name;
            }
        }
    }

    fn current_scope_mut(&mut self) -> &mut HashSet<String> {
        self.lexical_scopes
            .last_mut()
            .expect("lexical_scopes is initialised non-empty and never popped past 1")
    }

    fn bind_pattern(&mut self, pat: &Pattern) {
        pat.collect_names(self.current_scope_mut());
    }

    /// Translate an AST pattern into an [`IrPattern`].  Callers must elaborate
    /// the pattern *before* its own names enter scope, so a map default like
    /// `[host: h = $h]` resolves `$h` outward rather than to itself.
    fn elab_pattern(&mut self, pat: &Pattern) -> IrPattern {
        match pat {
            Pattern::Wildcard => IrPattern::Wildcard,
            Pattern::Name(n) => {
                if let Some((_, reading)) = reserved_register(n) {
                    self.error.get_or_insert_with(|| ParseError {
                        message: format!(
                            "${n} is a reading of {reading}, not a name you can bind; \
                             choose another name"
                        ),
                        span: self.current_span,
                        lex_kind: None,
                        incompleteness: None,
                    });
                }
                IrPattern::Name(n.clone())
            }
            Pattern::List { elems, rest } => IrPattern::List {
                elems: elems.iter().map(|e| self.elab_pattern(e)).collect(),
                rest: rest.clone(),
            },
            Pattern::Map(entries) => IrPattern::Map(
                entries
                    .iter()
                    .map(|entry| {
                        let pattern = self.elab_pattern(&entry.pattern);
                        let default = entry.default.as_ref().map(|d| {
                            // A default is statement-shaped but carries no span
                            // of its own, so it inherits the pattern's.
                            let stmt = [Spanned::with_span(self.current_span, d.clone())];
                            Arc::new(self.stmts_nested(&stmt))
                        });
                        MapPatternEntry {
                            key: entry.key.clone(),
                            pattern,
                            default,
                        }
                    })
                    .collect(),
            ),
        }
    }

    /// A `{ |param| body }` binder together with the statements it scopes: the
    /// param elaborates *before* its names enter scope, so its defaults resolve
    /// outward, and the body is then elaborated inside the frame those names
    /// open.  Both readings of that spelling — the lambda it denotes and the
    /// `case` arm that is a branch rather than a function — get their scope from
    /// here, so the ordering is stated once.
    fn elab_binder_scope(
        &mut self,
        param: &Spanned<ast::Param>,
        body: &[Stmt],
    ) -> (IrPattern, Comp) {
        let pattern = self.with_span(param.span, |this| this.elab_pattern(&param.item));
        let mut names = HashSet::new();
        param.item.collect_names(&mut names);
        let body = self.with_bound_names(names, |this| this.stmts_nested(body));
        (pattern, body)
    }

    fn with_bound_names<T>(
        &mut self,
        names: impl IntoIterator<Item = String>,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let saved_span = self.current_span;
        self.lexical_scopes.push(names.into_iter().collect());
        let out = f(self);
        self.lexical_scopes.pop();
        self.current_span = saved_span;
        out
    }

    /// For block and branch bodies, which introduce no names of their own but
    /// still need a frame so their `let`s shadow rather than leak outward.
    fn with_new_scope<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.with_bound_names(std::iter::empty::<String>(), f)
    }

    fn is_bound(&self, name: &str) -> bool {
        self.lexical_scopes
            .iter()
            .rev()
            .any(|scope| scope.contains(name))
            || self.prelude.contains(name)
    }

    /// Every name-dispatched head (`bare`, `^name`, `./path`, `~/path`) funnels
    /// through here.
    fn exec(
        &self,
        name: CommandName,
        args: Args,
        redirects: Vec<RedirectV>,
        external_only: bool,
    ) -> Comp {
        let head = if external_only {
            CommandWord::External(name)
        } else {
            CommandWord::Name(name)
        };
        comp!(
            self,
            CompKind::Exec(Exec {
                head,
                args,
                redirects,
            })
        )
    }

    /// A non-`let`, non-`source` statement's own `Comp`: the hoisting
    /// boundary, folding whatever the subtree pushed into `binds` into a
    /// `Comp::Bind` chain.
    fn stmt(&mut self, ast: &Ast) -> Comp {
        let mut binds = Vec::new();
        let comp = self.elab_expr(ast, &mut binds);
        wrap_binds(self.current_span, binds, comp)
    }

    /// A `let pattern = value`'s two halves, each under its own span so a
    /// bind failure in `let [a, b] = 42` underlines `42` and a pattern-shape
    /// one underlines the pattern, not the statement.  The pattern's names
    /// are not yet in scope: the caller binds them once it decides where
    /// `let x = x` should read the outer `x` from.
    fn elab_let_parts(
        &mut self,
        pattern: &Spanned<Pattern>,
        value: &Spanned<Box<Ast>>,
    ) -> (Comp, IrPattern) {
        let mut binds = Vec::new();
        let comp = self.with_span(value.span, |this| this.elab_expr(&value.item, &mut binds));
        let pattern_ir = self.with_span(pattern.span, |this| this.elab_pattern(&pattern.item));
        // The temporaries wrap the right-hand side, not the `Bind`: only the
        // RHS reads them, and a frame around the `Bind` would take the
        // user's own binding down with them.
        let rhs = wrap_binds(self.current_span, binds, comp);
        (rhs, pattern_ir)
    }

    /// Forward-declares a recursive knot's own names, then elaborates each
    /// member's RHS to its thunk body, sharing one `group` `Arc` — Levy's
    /// `rec x⃗. M⃗`.  Confining the forward declaration to the group, rather
    /// than scanning ahead over earlier statements, keeps a preceding
    /// command use of the same name lowering to `Exec` instead of a
    /// dangling `Force(Variable)`.
    fn build_rec_group(
        &mut self,
        bindings: &[(String, Box<Ast>, Option<Span>)],
    ) -> Arc<[(String, Arc<Comp>)]> {
        let scope = self.current_scope_mut();
        for (name, _, _) in bindings {
            scope.insert(name.clone());
        }
        bindings
            .iter()
            .map(|(name, value, span)| {
                let mut empty = Vec::new();
                let CompKind::Return(Val::Thunk(arc)) = self
                    .with_span(*span, |this| this.elab_expr(value, &mut empty))
                    .item
                else {
                    unreachable!(
                        "group.rs only emits lambda/block LetRec RHS, \
                         which elaborate to Return(Thunk(_))"
                    )
                };
                debug_assert!(
                    empty.is_empty(),
                    "lambda/block elaboration must not hoist into outer binds"
                );
                (name.clone(), arc)
            })
            .collect()
    }

    /// `ast` is a bare, unbound `source path` — one argument, no redirects —
    /// the only call shape [`Phrase::Source`]/[`CompKind::Source`] lower
    /// from (S2).  A bound `source`, or any other shape, reaches the
    /// builtin table entry instead, so this returns the sole argument only
    /// when the form is that one shape.
    fn as_source_arg<'a>(&self, ast: &'a Ast) -> Option<&'a Spanned<Ast>> {
        let Ast::Call {
            head: Head::Bare(name),
            args,
            redirects,
        } = ast
        else {
            return None;
        };
        if name != "source" || args.len() != 1 || !redirects.is_empty() || self.is_bound("source")
        {
            return None;
        }
        args.first()
    }

    /// A `source` form's path argument as an `F String` computation: hoists
    /// of its own (a `~`-path, an interpolation, `$[…]`) wrap the returned
    /// value exactly as any statement's do.
    fn elab_source_path(&mut self, arg: &Spanned<Ast>) -> Comp {
        let mut binds = Vec::new();
        let v = self.with_span(arg.span, |this| this.to_val(&arg.item, &mut binds));
        let ret = comp!(self, CompKind::Return(v));
        wrap_binds(self.current_span, binds, ret)
    }

    /// One top-level phrase (§3.4, depth 0): a `let` becomes a `Define`, an
    /// unbound bare `source path` becomes a `Source`, anything else a `Run`.
    /// The `Define` pattern's names enter scope only after the RHS is
    /// elaborated, as `nested_single`'s `let` arm does.
    fn toplevel_phrase(&mut self, stmt: Stmt) -> Spanned<Phrase> {
        let Spanned { item: kind, span } = stmt;
        self.with_span(span, |this| {
            if let Ast::Let { pattern, value } = &kind {
                let (rhs, pattern_ir) = this.elab_let_parts(pattern, value);
                this.bind_pattern(&pattern.item);
                return Spanned::with_span(
                    span,
                    Phrase::Define {
                        pattern: Arc::new(pattern_ir),
                        comp: Arc::new(rhs),
                        schemes: vec![],
                    },
                );
            }
            if let Some(arg) = this.as_source_arg(&kind) {
                let path = this.elab_source_path(arg);
                return Spanned::with_span(
                    span,
                    Phrase::Source {
                        path: Arc::new(path),
                    },
                );
            }
            let mut binds = Vec::new();
            let comp = this.elab_expr(&kind, &mut binds);
            let comp = wrap_binds(this.current_span, binds, comp);
            Spanned::with_span(span, Phrase::Run(Arc::new(comp)))
        })
    }

    /// A recursive knot at depth 0: *n* `Define`s, `xᵢ = Return(Thunk(Rec{group,
    /// i}))`, sharing one `group` `Arc`.
    fn toplevel_rec_group(
        &mut self,
        bindings: &[(String, Box<Ast>, Option<Span>)],
    ) -> Vec<Spanned<Phrase>> {
        let group = self.build_rec_group(bindings);
        bindings
            .iter()
            .enumerate()
            .map(|(index, (name, _, span))| {
                let rec = Spanned::with_span(
                    *span,
                    CompKind::Rec {
                        group: group.clone(),
                        index,
                    },
                );
                let comp = Arc::new(Spanned::with_span(
                    *span,
                    CompKind::Return(Val::Thunk(Arc::new(rec))),
                ));
                Spanned::with_span(
                    *span,
                    Phrase::Define {
                        pattern: Arc::new(IrPattern::Name(name.clone())),
                        comp,
                        schemes: vec![],
                    },
                )
            })
            .collect()
    }

    /// Elaborate `stmts` at depth > 0 — every block or lambda body — right-
    /// nesting each statement into a `Bind`/`Source` chain over what follows
    /// it, so `{ a; b }` is `a to _. b` (S11).  The tail of the block is its
    /// last statement's own comp, not a `Bind` on it; a block ending in a
    /// `let`/`source` has tail `Return(Unit)`.
    fn stmts_nested(&mut self, stmts: &[Stmt]) -> Comp {
        let mut units: Vec<(Option<Span>, NestedUnit)> = Vec::new();
        for group in group_stmts(stmts) {
            match group {
                StmtGroup::Single(stmt) => units.push(self.nested_single(stmt)),
                StmtGroup::LetRec(bindings) => units.extend(self.nested_rec_group(&bindings)),
            }
        }
        let mut rev = units.into_iter().rev();
        let Some((span, last)) = rev.next() else {
            return comp!(self, CompKind::Return(Val::Unit));
        };
        let mut rest = nested_tail(span, last);
        for (span, unit) in rev {
            rest = nested_wrap(span, unit, rest);
        }
        rest
    }

    /// One statement at depth > 0: a `let` binds a fresh `Name` (never
    /// `Wildcard` — a surface `_` gets the hygienic gensym `$[…]` temporaries
    /// already use), a bare unbound `source path` is a `Source`, and
    /// anything else is a discard, marked `Wildcard`, which no surface
    /// pattern ever produces.
    fn nested_single(&mut self, stmt: Stmt) -> (Option<Span>, NestedUnit) {
        let Spanned { item: kind, span } = stmt;
        self.with_span(span, |this| {
            if let Ast::Let { pattern, value } = &kind {
                let (rhs, pattern_ir) = this.elab_let_parts(pattern, value);
                let pattern_ir = match pattern_ir {
                    IrPattern::Wildcard => IrPattern::Name(this.gensym()),
                    named => named,
                };
                this.bind_pattern(&pattern.item);
                return (
                    span,
                    NestedUnit::Bind {
                        rhs,
                        pattern: pattern_ir,
                    },
                );
            }
            if let Some(arg) = this.as_source_arg(&kind) {
                let path = this.elab_source_path(arg);
                return (span, NestedUnit::Source { path });
            }
            let comp = this.stmt(&kind);
            (span, NestedUnit::Other { comp })
        })
    }

    /// A recursive knot at depth > 0: *n* nested `Bind`s over `Return(Thunk(
    /// Rec{group, i}))`, sharing one `group` `Arc`, in source order.
    fn nested_rec_group(
        &mut self,
        bindings: &[(String, Box<Ast>, Option<Span>)],
    ) -> Vec<(Option<Span>, NestedUnit)> {
        let group = self.build_rec_group(bindings);
        bindings
            .iter()
            .enumerate()
            .map(|(index, (name, _, span))| {
                let rec = Spanned::with_span(
                    *span,
                    CompKind::Rec {
                        group: group.clone(),
                        index,
                    },
                );
                let rhs = Spanned::with_span(*span, CompKind::Return(Val::Thunk(Arc::new(rec))));
                (
                    *span,
                    NestedUnit::Bind {
                        rhs,
                        pattern: IrPattern::Name(name.clone()),
                    },
                )
            })
            .collect()
    }

    /// Elaborate `ast` as a computation, pushing every sub-expression that must
    /// run before its parent into `binds` for the caller to `wrap_binds`.
    fn elab_expr(&mut self, ast: &Ast, binds: &mut Vec<(IrPattern, Comp)>) -> Comp {
        match ast {
            Ast::Word(Word::Plain(s) | Word::Slash(s)) => {
                comp!(self, CompKind::Return(Val::from_word(s)))
            }
            Ast::Literal(s) => comp!(self, CompKind::Return(Val::String(s.clone()))),
            Ast::Variable(s) => {
                let v = if let Some((register, _)) = reserved_register(s) {
                    let observed = comp!(self, CompKind::Observe(register));
                    self.hoist(observed, binds)
                } else {
                    self.variable_val(s)
                };
                comp!(self, CompKind::Return(v))
            }
            Ast::Word(Word::Tilde(path)) => {
                let observed = comp!(self, CompKind::Observe(Register::Tilde(path.clone())));
                let v = self.hoist(observed, binds);
                comp!(self, CompKind::Return(v))
            }

            Ast::Block(body) => {
                let body_comp = self.with_new_scope(|this| this.stmts_nested(body));
                comp!(self, CompKind::Return(Val::Thunk(Arc::new(body_comp))))
            }

            Ast::Lambda { param, body } => {
                let (param_ir, body_comp) = self.elab_binder_scope(param, body);
                // A body that is itself a single lambda already carries its own
                // thunk; reuse it rather than wrapping a thunk in a thunk.
                let body_arc: Arc<Comp> = match &body_comp.item {
                    CompKind::Return(Val::Thunk(inner))
                        if matches!(inner.as_ref().item, CompKind::Lam { .. }) =>
                    {
                        Arc::clone(inner)
                    }
                    _ => Arc::new(body_comp),
                };
                comp!(
                    self,
                    CompKind::Return(Val::Thunk(Arc::new(comp!(
                        self,
                        CompKind::Lam {
                            param: param_ir,
                            body: body_arc,
                        }
                    ))))
                )
            }

            Ast::Force(value) => self.with_span(value.span, |this| {
                comp!(this, CompKind::Force(this.to_val(&value.item, binds)))
            }),

            Ast::Call {
                head,
                args,
                redirects,
            } => {
                // A value head can hoist binds of its own, and its effects
                // precede the arguments' in source order — so it is elaborated
                // before the args below, or its binds land after theirs.
                let value_head_comp = if let Head::Value(value) = head {
                    // A block literal in head position stays `Return(Thunk(…))`
                    // — a value, so nothing consumes the redirect.  A bound or
                    // forced head gets the `Force` below, which the redirect
                    // does bracket; hence a warning rather than an error.
                    if matches!(value.as_ref(), Ast::Block(_)) && !redirects.is_empty() {
                        crate::diagnostic::shell_warning(
                            "redirect on a `{ … }` literal: the block is a \
                             value, not a command — the redirect has no \
                             consumer.  Bind first (`let f = { … }; f < file`) \
                             or force (`!{ … } < file`).",
                        );
                    }
                    Some(self.elab_expr(value, binds))
                } else {
                    None
                };

                // Only an argument-position spread splices here: a list-literal
                // argument `f [...$xs]` stays one arg carrying its own spread.
                let arg_vals: Args = args
                    .iter()
                    .map(|a| {
                        let elem = match &a.item {
                            Ast::Spread(inner) => {
                                ValListElem::Spread(self.to_val(&inner.item, binds))
                            }
                            other => ValListElem::Single(self.to_val(other, binds)),
                        };
                        Spanned::with_span(a.span, elem)
                    })
                    .collect();
                let redirect_vals = self.lower_redirects(redirects, binds);

                match head {
                    Head::ExternalName(s) => {
                        self.exec(CommandName::Bare(s.clone()), arg_vals, redirect_vals, true)
                    }
                    Head::Bare(s) if self.is_bound(s) => {
                        // The `Force` is what makes the `Force` rule run a
                        // bound block inside the redirect frame wrapped
                        // around it.
                        let head_comp = comp!(self, CompKind::Force(Val::Variable(s.clone())));
                        self.apply_head(head_comp, arg_vals, redirect_vals)
                    }
                    Head::Bare(s) => self.exec(
                        CommandName::Bare(s.clone()),
                        desugar_zero_arg_exit(s, arg_vals),
                        redirect_vals,
                        false,
                    ),
                    Head::Path(path) => self.exec(
                        CommandName::Path(path.clone()),
                        arg_vals,
                        redirect_vals,
                        false,
                    ),
                    Head::TildePath(path) => self.exec(
                        CommandName::TildePath(path.clone()),
                        arg_vals,
                        redirect_vals,
                        false,
                    ),
                    Head::Value(_) => {
                        let head_comp = value_head_comp.expect("computed above for Head::Value");
                        self.apply_head(head_comp, arg_vals, redirect_vals)
                    }
                }
            }

            Ast::Scope { op, redirects } => {
                let redirect_vals = self.lower_redirects(redirects, binds);
                let inner = match op {
                    ScopeAst::Try { body, handler } => comp!(
                        self,
                        CompKind::Try {
                            body: self.to_val(body, binds),
                            handler: self.to_val(handler, binds),
                        }
                    ),
                    ScopeAst::Guard { body, cleanup } => comp!(
                        self,
                        CompKind::Guard {
                            body: self.to_val(body, binds),
                            cleanup: self.to_val(cleanup, binds),
                        }
                    ),
                    ScopeAst::Within { opts, body } => comp!(
                        self,
                        CompKind::Within {
                            opts: self.to_val(opts, binds),
                            body: self.to_val(body, binds),
                        }
                    ),
                    ScopeAst::Grant { caps, body } => comp!(
                        self,
                        CompKind::Grant {
                            caps: self.to_val(caps, binds),
                            body: self.to_val(body, binds),
                        }
                    ),
                    ScopeAst::Audit { body } => comp!(
                        self,
                        CompKind::Audit {
                            body: self.to_val(body, binds),
                        }
                    ),
                };
                self.wrap_redirect(inner, redirect_vals)
            }

            // `return` with no value and `()` name the same value.
            Ast::Unit | Ast::Return(None) => comp!(self, CompKind::Return(Val::Unit)),

            Ast::Return(Some(value)) => self.with_span(value.span, |this| {
                comp!(this, CompKind::Return(this.to_val(&value.item, binds)))
            }),

            Ast::Pipeline(stages) => {
                let mut comps = Vec::new();
                for stage in stages {
                    // A `{ … }` stage is the thunk the pipeline drives, not
                    // inline statements — hence `elab_isolated`, which isolates
                    // the stage's hoists without `elab_guarded`'s inline-block
                    // reading.
                    let stage_comp =
                        self.with_span(stage.span, |this| this.elab_isolated(&stage.item));
                    comps.push(Arc::new(stage_comp));
                }
                // Placeholders, overwritten by the annotation pass with the
                // stages' value types and the pipeline's yield.  The checker
                // runs before every evaluation, so an un-annotated pipeline
                // never reaches the evaluator.
                let stage_types = vec![crate::typecheck::Ty::Unit; comps.len()];
                comp!(
                    self,
                    CompKind::Pipeline {
                        stages: comps,
                        stage_types,
                        yields: PipeYield::Last,
                    }
                )
            }

            Ast::Chain(parts) => {
                // Every arm but the first runs only when its predecessors
                // failed, so each needs the fresh binds vector `elab_guarded`
                // gives it.
                comp!(
                    self,
                    CompKind::Chain(
                        parts
                            .iter()
                            .map(|a| Arc::new(
                                self.with_span(a.span, |this| { this.elab_guarded(&a.item) })
                            ))
                            .collect()
                    )
                )
            }

            Ast::List(elems) => comp!(
                self,
                CompKind::Return(Val::List(
                    elems
                        .iter()
                        .map(|e| match e {
                            ListElem::Single(a) => ValListElem::Single(
                                self.with_span(a.span, |this| this.to_val(&a.item, binds),)
                            ),
                            ListElem::Spread(a) => ValListElem::Spread(
                                self.with_span(a.span, |this| this.to_val(&a.item, binds),)
                            ),
                        })
                        .collect(),
                ))
            ),

            Ast::Map(entries) => comp!(
                self,
                CompKind::Return(Val::Map(
                    entries
                        .iter()
                        .map(|e| match e {
                            MapEntry::Entry { key, value } => ValMapEntry::Entry(
                                Val::String(key.row_label()),
                                self.with_span(value.span, |this| this.to_val(&value.item, binds)),
                            ),
                            MapEntry::Deref { name, value } => ValMapEntry::Entry(
                                self.variable_val(name),
                                self.with_span(value.span, |this| this.to_val(&value.item, binds)),
                            ),
                            MapEntry::Spread(a) => ValMapEntry::Spread(
                                self.with_span(a.span, |this| this.to_val(&a.item, binds),)
                            ),
                        })
                        .collect(),
                ))
            ),

            Ast::Tag { label, payload } => {
                let payload_val = payload
                    .as_ref()
                    .map(|p| self.with_span(p.span, |this| Box::new(this.to_val(&p.item, binds))));
                comp!(
                    self,
                    CompKind::Return(Val::Variant {
                        label: label.clone(),
                        payload: payload_val,
                    })
                )
            }

            Ast::Interpolation(parts) => {
                comp!(
                    self,
                    CompKind::Interpolation(
                        parts
                            .iter()
                            .map(|a| self.with_span(a.span, |this| this.to_val(&a.item, binds)))
                            .collect()
                    )
                )
            }

            Ast::Expr(expr) => self.lower_expr(expr, binds),

            Ast::Index { target, keys } => comp!(
                self,
                CompKind::Index {
                    target: self
                        .with_span(target.span, |this| { this.to_val(&target.item, binds) }),
                    keys: keys
                        .iter()
                        .map(|k| Spanned::with_span(
                            k.span,
                            self.with_span(k.span, |this| this.to_val(&k.item, binds)),
                        ))
                        .collect(),
                }
            ),

            Ast::If { branches, else_ } => self.elab_if(branches, else_.as_ref(), binds),

            // The scrutinee always runs, so whatever it hoists joins the
            // caller's binds; an arm may not run, and its body is a closed
            // computation that hoists nothing outward.
            Ast::Case { scrutinee, arms } => comp!(
                self,
                CompKind::Case {
                    scrutinee: Spanned::with_span(
                        scrutinee.span,
                        self.with_span(scrutinee.span, |this| {
                            this.to_val(&scrutinee.item, binds)
                        }),
                    ),
                    arms: arms.iter().map(|arm| self.elab_case_arm(arm)).collect(),
                }
            ),

            Ast::Let { .. } => unreachable!("assignment in elab_expr"),

            Ast::Spread(_) => {
                unreachable!("Ast::Spread must be consumed by Ast::Call's arg lowering")
            }
        }
    }

    /// Nest `if`/`elsif`/`else` into `CompKind::If`.  The first cond always
    /// runs, so it hoists into the caller's `binds`; every later cond gets a
    /// local vector wrapped around the else-arm that guards it.
    fn elab_if(
        &mut self,
        branches: &[IfBranch],
        else_: Option<&Spanned<Box<Ast>>>,
        binds: &mut Vec<(IrPattern, Comp)>,
    ) -> Comp {
        let (first, rest) = branches
            .split_first()
            .expect("if must have at least one branch");
        let one_armed = rest.is_empty() && else_.is_none();

        let mut else_comp = match else_ {
            Some(e) => self.elab_branch(&e.item),
            None => Arc::new(comp!(self, CompKind::Return(Val::Unit))),
        };
        for branch in rest.iter().rev() {
            let mut local_binds = Vec::new();
            let cond_val = self.to_val(&branch.cond.item, &mut local_binds);
            let then_comp = self.elab_branch(&branch.body.item);
            let nested = comp!(
                self,
                CompKind::If {
                    cond: Spanned::with_span(branch.cond.span, cond_val),
                    then: then_comp,
                    else_: else_comp,
                }
            );
            else_comp = Arc::new(wrap_binds(self.current_span, local_binds, nested));
        }

        let cond_val = self.to_val(&first.cond.item, binds);
        let then_comp = if one_armed {
            self.elab_branch_unit(&first.body.item)
        } else {
            self.elab_branch(&first.body.item)
        };
        comp!(
            self,
            CompKind::If {
                cond: Spanned::with_span(first.cond.span, cond_val),
                then: then_comp,
                else_: else_comp,
            }
        )
    }

    fn elab_branch(&mut self, ast: &Ast) -> Arc<Comp> {
        Arc::new(self.elab_guarded(ast))
    }

    /// One `case` arm, in either spelling, as the same `pattern`-and-`body`
    /// branch.
    ///
    /// An arm written `{ |p| … }` *is* that branch: the binder elaborates
    /// before its names enter scope, as a lambda parameter does, and the body
    /// runs inline, so tail position passes through and nothing it binds
    /// escapes.  Any other atom is the function it names applied to the
    /// payload — elaborated from the very call the user could have written, so
    /// the two spellings agree on type, route, and coercion by construction.
    fn elab_case_arm(&mut self, arm: &ast::CaseArm) -> CaseArm {
        let tag = arm.tag.clone();
        self.with_span(arm.body.span, |this| match arm.body.item.as_ref() {
            Ast::Lambda { param, body } => {
                let (pattern, body) = this.elab_binder_scope(param, body);
                CaseArm {
                    tag,
                    pattern,
                    body: ArmBody::Inline(Arc::new(body)),
                }
            }
            handler => {
                let payload = this.gensym();
                let call = Ast::Call {
                    head: Head::Value(Box::new(handler.clone())),
                    args: vec![Spanned::with_span(
                        arm.body.span,
                        Ast::Variable(payload.clone()),
                    )],
                    redirects: Vec::new(),
                };
                CaseArm {
                    tag,
                    pattern: IrPattern::Name(payload),
                    body: ArmBody::Applied(Arc::new(this.elab_guarded(&call))),
                }
            }
        })
    }

    /// Elaborate `ast` in a context that may not run it.  The fresh binds
    /// vector is the whole point: whatever `ast` hoists is wrapped inside the
    /// returned `Comp`, never threaded into the caller's accumulator, so an
    /// untaken arm's effects physically cannot escape the guard.  A surface
    /// `{ … }` here means "run these statements inline"; tail position
    /// propagates through unchanged, so an inline branch still tail-calls.
    fn elab_guarded(&mut self, ast: &Ast) -> Comp {
        let mut branch_binds = Vec::new();
        let body = match ast {
            Ast::Block(stmts) => self.with_new_scope(|this| this.stmts_nested(stmts)),
            _ => self.elab_expr(ast, &mut branch_binds),
        };
        wrap_binds(self.current_span, branch_binds, body)
    }

    /// Isolates hoists like [`Self::elab_guarded`], but reads a surface `{ … }`
    /// as the thunk value it denotes: a pipeline stage is data the pipeline
    /// drives, with every interior handoff carried as bytes. Reading it inline
    /// would splice the stage's statements into the surrounding computation.
    fn elab_isolated(&mut self, ast: &Ast) -> Comp {
        let mut stage_binds = Vec::new();
        let body = self.elab_expr(ast, &mut stage_binds);
        wrap_binds(self.current_span, stage_binds, body)
    }

    /// Discards the branch's result, so a one-armed `if` types as `F Unit`:
    /// `body to _. return ()`.
    fn elab_branch_unit(&mut self, ast: &Ast) -> Arc<Comp> {
        let body = self.elab_branch(ast);
        Arc::new(comp!(
            self,
            CompKind::Bind {
                comp: body,
                pattern: Arc::new(IrPattern::Wildcard),
                rest: Arc::new(comp!(self, CompKind::Return(Val::Unit))),
            }
        ))
    }

    /// Yield the `Val` the parent consumes: `Return(v)` passes through, and
    /// anything else is bound to a fresh `_gN` pushed onto `binds`.
    fn hoist(&mut self, comp: Comp, binds: &mut Vec<(IrPattern, Comp)>) -> Val {
        if let CompKind::Return(v) = comp.item {
            v
        } else {
            let name = self.gensym();
            binds.push((IrPattern::Name(name.clone()), comp));
            Val::Variable(name)
        }
    }

    #[allow(clippy::wrong_self_convention)]
    fn to_val(&mut self, ast: &Ast, binds: &mut Vec<(IrPattern, Comp)>) -> Val {
        let comp = self.elab_expr(ast, binds);
        self.hoist(comp, binds)
    }

    /// Shared by the two value-application heads, a bound bare name (`f x`) and
    /// an explicit value head (`$f x`, `{…} x`).  A zero-arg call is the head
    /// computation alone: `App` with an empty argument list is not a CBPV form.
    fn apply_head(&self, head_comp: Comp, arg_vals: Args, redirects: Vec<RedirectV>) -> Comp {
        let app = if arg_vals.is_empty() {
            head_comp
        } else {
            comp!(
                self,
                CompKind::App {
                    head: Arc::new(head_comp),
                    args: arg_vals,
                }
            )
        };
        self.wrap_redirect(app, redirects)
    }

    /// Attach trailing `redirects` to `body` as a [`CompKind::Redirect`] frame.
    /// `Exec` fuses its redirects into the syscall instead, and pipelines and
    /// chains take none at the surface, so this covers every remaining body.
    fn wrap_redirect(&self, body: Comp, redirects: Vec<RedirectV>) -> Comp {
        if redirects.is_empty() {
            return body;
        }
        comp!(
            self,
            CompKind::Redirect {
                body: Arc::new(body),
                redirects,
            }
        )
    }

    /// Lower parser-side [`Redirect`]s to IR [`RedirectV`]s, hoisting effectful
    /// targets into `binds` like any other value.
    fn lower_redirects(
        &mut self,
        redirects: &[Redirect],
        binds: &mut Vec<(IrPattern, Comp)>,
    ) -> Vec<RedirectV> {
        redirects
            .iter()
            .map(|r| {
                let target = match &r.target {
                    RedirectTarget::File(a) => ValRedirectTarget::File(self.to_val(a, binds)),
                    RedirectTarget::Fd(n) => ValRedirectTarget::Fd(*n),
                };
                RedirectV {
                    fd: r.fd,
                    mode: r.mode,
                    target,
                }
            })
            .collect()
    }

    /// Lower an `Ast::Expr` into CBPV primitives — there is no expression IR,
    /// so `$[a + b > 0]` unfolds into a flat `Comp::Bind` chain over
    /// primitive-operator leaves at the enclosing statement boundary.
    fn lower_expr(&mut self, expr: &Expr, binds: &mut Vec<(IrPattern, Comp)>) -> Comp {
        match expr {
            Expr::Integer(n) => comp!(self, CompKind::Return(Val::Int(*n))),
            Expr::Number(n) => comp!(self, CompKind::Return(Val::Float(*n))),
            Expr::Bool(b) => comp!(self, CompKind::Return(Val::Bool(*b))),
            Expr::Variable(name) => {
                let v = self.variable_val(name);
                comp!(self, CompKind::Return(v))
            }
            Expr::Index(name, keys) => {
                let target = self.variable_val(name);
                comp!(
                    self,
                    CompKind::Index {
                        target,
                        keys: keys
                            .iter()
                            .map(|k| Spanned::with_span(
                                k.span,
                                self.with_span(k.span, |this| this.to_val(&k.item, binds)),
                            ))
                            .collect(),
                    }
                )
            }
            Expr::Force(inner) => self.with_span(inner.span, |this| {
                comp!(this, CompKind::Force(this.to_val(&inner.item, binds)))
            }),
            Expr::BinOp(l, op, r) => {
                let lv = self.expr_to_val(l, binds);
                let rv = self.expr_to_val(r, binds);
                comp!(self, CompKind::Binary(*op, lv, rv))
            }
            Expr::Negate(inner) => {
                let v = self.expr_to_val(inner, binds);
                comp!(self, CompKind::Negate(v))
            }
            Expr::Not(inner) => {
                let v = self.expr_to_val(inner, binds);
                comp!(self, CompKind::Not(v))
            }
            Expr::And(l, r) => self.lower_short_circuit(l, r, binds, /*on_true_is_rhs=*/ true),
            Expr::Or(l, r) => self.lower_short_circuit(l, r, binds, /*on_true_is_rhs=*/ false),
        }
    }

    /// `to_val` for `Expr` instead of `Ast`.
    fn expr_to_val(&mut self, expr: &Expr, binds: &mut Vec<(IrPattern, Comp)>) -> Val {
        let c = self.lower_expr(expr, binds);
        self.hoist(c, binds)
    }

    /// Desugar `a && b` / `a || b` into an `If`.  The RHS runs only
    /// conditionally, so it lowers in an isolated `binds` vector.
    fn lower_short_circuit(
        &mut self,
        l: &Expr,
        r: &Expr,
        binds: &mut Vec<(IrPattern, Comp)>,
        on_true_is_rhs: bool,
    ) -> Comp {
        let cond = self.expr_to_val(l, binds);
        let mut r_binds = Vec::new();
        let r_comp = self.lower_expr(r, &mut r_binds);
        let r_comp = wrap_binds(self.current_span, r_binds, r_comp);
        let short = comp!(self, CompKind::Return(Val::Bool(!on_true_is_rhs)));
        let (then_branch, else_branch) = if on_true_is_rhs {
            (r_comp, short)
        } else {
            (short, r_comp)
        };
        comp!(
            self,
            CompKind::If {
                // No surface token holds this cond, so it carries no span and
                // diagnostics fall back to the enclosing one.
                cond: Spanned::synthetic(cond),
                then: Arc::new(then_branch),
                else_: Arc::new(else_branch),
            }
        )
    }
}

/// Fold `binds` into a chain of `Comp::Bind` nodes around `inner`, the first
/// binding outermost so the chain runs in the order the hoists were pushed.
fn wrap_binds(span: Option<Span>, binds: Vec<(IrPattern, Comp)>, inner: Comp) -> Comp {
    binds.into_iter().rev().fold(inner, |rest, (pattern, comp)| {
        Spanned::with_span(
            span,
            CompKind::Bind {
                comp: Arc::new(comp),
                pattern: Arc::new(pattern),
                rest: Arc::new(rest),
            },
        )
    })
}

/// One statement of a block, elaborated but not yet nested over what
/// follows it — [`Elaborator::stmts_nested`]'s intermediate form.
enum NestedUnit {
    /// A `let` or a recursive group's member: `rest` is the elaboration of
    /// what follows.
    Bind { rhs: Comp, pattern: IrPattern },
    /// A bare `source path`: `rest` is what follows.
    Source { path: Comp },
    /// Any other statement, discarded on a `Wildcard` bind.
    Other { comp: Comp },
}

/// The last unit of a block: a `let`/`source` still needs a `rest` — the
/// block's own value, `Unit` — but a plain statement's comp *is* the tail.
fn nested_tail(span: Option<Span>, unit: NestedUnit) -> Comp {
    match unit {
        NestedUnit::Other { comp } => comp,
        other => nested_wrap(span, other, Spanned::with_span(span, CompKind::Return(Val::Unit))),
    }
}

/// Nest one [`NestedUnit`] over `rest`.
fn nested_wrap(span: Option<Span>, unit: NestedUnit, rest: Comp) -> Comp {
    match unit {
        NestedUnit::Bind { rhs, pattern } => Spanned::with_span(
            span,
            CompKind::Bind {
                comp: Arc::new(rhs),
                pattern: Arc::new(pattern),
                rest: Arc::new(rest),
            },
        ),
        NestedUnit::Source { path } => Spanned::with_span(
            span,
            CompKind::Source {
                path: Arc::new(path),
                rest: Arc::new(rest),
            },
        ),
        NestedUnit::Other { comp } => Spanned::with_span(
            span,
            CompKind::Bind {
                comp: Arc::new(comp),
                pattern: Arc::new(IrPattern::Wildcard),
                rest: Arc::new(rest),
            },
        ),
    }
}

/// The five reserved pseudo-variables (S8): a store read, and the English
/// name of what it reads, for the "not a name you can bind" diagnostic.
fn reserved_register(name: &str) -> Option<(Register, &'static str)> {
    Some(match name {
        "ENV" => (Register::Env, "the shell's environment"),
        "ARGS" => (Register::Args, "the shell's arguments"),
        "NPROC" => (Register::Nproc, "the machine's processor count"),
        "CWD" => (Register::Cwd, "the shell's working directory"),
        "USER" => (Register::User, "the current user"),
        _ => return None,
    })
}

/// Sugar bare `exit` / `quit` into `exit 0` / `quit 0`.  The builtin tolerates
/// zero args, but ral's fixed-arity rule gives it one `Int` slot; supplying the
/// status here spares the typechecker a zero-arg special case.
fn desugar_zero_arg_exit(name: &str, args: Args) -> Args {
    if args.is_empty() && (name == "exit" || name == "quit") {
        vec![Spanned::synthetic(ValListElem::Single(Val::Int(0)))]
    } else {
        args
    }
}

/// The prelude's exported names, built once and shared by refcount thereafter.
fn prelude_scope() -> Arc<HashSet<String>> {
    static PRELUDE: std::sync::OnceLock<Arc<HashSet<String>>> = std::sync::OnceLock::new();
    PRELUDE
        .get_or_init(|| {
            Arc::new(
                prelude_manifest::PRELUDE_EXPORTS
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect(),
            )
        })
        .clone()
}

/// Elaborate a top-level statement sequence into a [`Toplevel`] (§3.4).
///
/// Each `let` becomes a `Define`, a `let`-knot becomes one `Define` per
/// member sharing a `Rec` group, an unbound bare `source path` becomes a
/// `Source`, and everything else a `Run`.
///
/// `bindings` are the names already live in the calling environment (a REPL's
/// accumulated definitions, say); the prelude is always in scope.  `name` is the
/// source's own display name, the value `$SCRIPT` resolves to.  Setting
/// `RAL_DUMP_IR` dumps the result to stderr on the way out.
///
/// # Errors
/// `$SCRIPT` referenced where `name` carries no script identity; a pattern
/// binding one of the five reserved pseudo-variable names.
#[allow(
    clippy::implicit_hasher,
    reason = "elaboration entry point; every caller passes a default HashSet of REPL/prelude bindings, so generalizing over the hasher would be signature ceremony with no call site to exercise it."
)]
pub fn elaborate(ast: &[Stmt], bindings: HashSet<String>, name: &str) -> Result<Toplevel, ParseError> {
    let mut elaborator = Elaborator::new_with_bindings(bindings, name);
    let mut phrases = Vec::new();
    for group in group_stmts(ast) {
        match group {
            StmtGroup::Single(stmt) => phrases.push(elaborator.toplevel_phrase(stmt)),
            StmtGroup::LetRec(rec) => phrases.extend(elaborator.toplevel_rec_group(&rec)),
        }
    }
    if let Some(e) = elaborator.error {
        return Err(e);
    }
    if std::env::var("RAL_DUMP_IR").is_ok() {
        eprintln!("{phrases:#?}");
    }
    Ok(Toplevel { phrases })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::parser::parse;

    /// Head name, args, and whether the head was written `^name`.
    fn expect_exec_name(comp: &Comp) -> (&CommandName, &Args, bool) {
        let CompKind::Exec(e) = &comp.item else {
            panic!("expected exec, got {:?}", comp.item);
        };
        let external_only = matches!(e.head, CommandWord::External(_));
        (e.head.name(), &e.args, external_only)
    }

    /// Strip spans so assertions match on shape alone.
    fn arg_items(args: &Args) -> Vec<ValListElem> {
        args.iter().map(|s| s.item.clone()).collect()
    }

    /// Elaborate one statement, unwrapped from its sole `Run` phrase — for
    /// tests over a single non-`let`, non-`source` statement.
    fn elaborate_one(ast: &[Stmt], bindings: HashSet<String>, name: &str) -> Arc<Comp> {
        let top = elaborate(ast, bindings, name).expect("elaborate");
        let [phrase] = top.phrases.as_slice() else {
            panic!("expected one phrase, got {:?}", top.phrases);
        };
        let Phrase::Run(comp) = &phrase.item else {
            panic!("expected a Run phrase, got {:?}", phrase.item);
        };
        comp.clone()
    }

    #[test]
    fn tilde_path_command_head_elaborates_to_exec() {
        let ast = parse("~/.local/bin/claude update").expect("parse");
        let comp = elaborate_one(&ast, HashSet::new(), "");
        let (name, args, _) = expect_exec_name(&comp);
        assert_eq!(
            name,
            &CommandName::TildePath(crate::path::tilde::TildePath {
                user: None,
                suffix: Some("/.local/bin/claude".into()),
            })
        );
        assert_eq!(
            arg_items(args),
            vec![ValListElem::Single(Val::String("update".into()))]
        );
    }

    #[test]
    fn tilde_path_command_head_without_args_elaborates_to_exec() {
        let ast = parse("~/.local/bin/claude").expect("parse");
        let comp = elaborate_one(&ast, HashSet::new(), "");
        let (name, args, _) = expect_exec_name(&comp);
        assert_eq!(
            name,
            &CommandName::TildePath(crate::path::tilde::TildePath {
                user: None,
                suffix: Some("/.local/bin/claude".into()),
            })
        );
        assert!(args.is_empty());
    }

    #[test]
    fn literal_path_head_elaborates_to_direct_exec() {
        let ast = parse("./script").expect("parse");
        let comp = elaborate_one(&ast, HashSet::new(), "");
        let (name, args, _) = expect_exec_name(&comp);
        assert_eq!(name, &CommandName::Path("./script".into()));
        assert!(args.is_empty());
    }

    #[test]
    fn external_name_head_elaborates_to_external_exec() {
        let ast = parse("^git status").expect("parse");
        let comp = elaborate_one(&ast, HashSet::new(), "");
        let (name, args, external_only) = expect_exec_name(&comp);
        assert_eq!(name, &CommandName::Bare("git".into()));
        assert_eq!(
            arg_items(args),
            vec![ValListElem::Single(Val::String("status".into()))]
        );
        assert!(external_only);
    }

    #[test]
    fn explicit_value_head_elaborates_to_app() {
        // A value head takes no wrapping `Force`: `apply` forces a thunk in
        // head position at runtime, which leaves a `<file` redirect on the
        // `App` free to bracket the body.
        let ast = parse("$map $upper ['a']").expect("parse");
        let comp = elaborate_one(&ast, HashSet::new(), "");
        let CompKind::App { head, args } = &comp.item else {
            panic!("expected app, got {:?}", comp.item);
        };
        let CompKind::Return(Val::Variable(name)) = &head.item else {
            panic!("expected returned-variable head, got {:?}", head.item);
        };
        assert_eq!(name, "map");
        assert_eq!(
            arg_items(args),
            vec![
                ValListElem::Single(Val::Variable("upper".into())),
                ValListElem::Single(Val::List(vec![ValListElem::Single(Val::String(
                    "a".into()
                ))])),
            ]
        );
    }

    /// Only thunk-form bindings are forward-declared — the shapes
    /// `syntax::group` knots — so a command use preceding a non-thunk `let` on
    /// the same name stays an `Exec`, not a `Force` of an unbound variable.
    #[test]
    fn command_use_before_non_thunk_let_is_exec() {
        let ast = parse("date\nlet date = 5").expect("parse");
        let top = elaborate(&ast, HashSet::new(), "").expect("elaborate");
        assert_eq!(top.phrases.len(), 2);
        let Phrase::Run(comp) = &top.phrases[0].item else {
            panic!("expected a Run phrase, got {:?}", top.phrases[0].item);
        };
        let (name, _, _) = expect_exec_name(comp);
        assert_eq!(name, &CommandName::Bare("date".into()));
    }

    /// An acyclic singleton `let f = { return 1 }` emits as a `Single`, whose
    /// `Bind` runs after an earlier use of `f` — so that use must be an `Exec`.
    #[test]
    fn command_use_before_acyclic_thunk_let_is_exec() {
        let ast = parse("f\nlet f = { return 1 }").expect("parse");
        let top = elaborate(&ast, HashSet::new(), "").expect("elaborate");
        assert_eq!(top.phrases.len(), 2);
        let Phrase::Run(comp) = &top.phrases[0].item else {
            panic!("expected a Run phrase, got {:?}", top.phrases[0].item);
        };
        let (name, _, _) = expect_exec_name(comp);
        assert_eq!(name, &CommandName::Bare("f".into()));
    }

    /// A use of `g` ahead of its self-recursive definition still lowers to
    /// `Exec`, while the self-reference inside the group resolves to the
    /// forward-declared binding.
    #[test]
    fn command_use_before_recursive_thunk_let_is_exec() {
        let ast = parse("g 3\nlet g = { |n| g $[$n - 1] }").expect("parse");
        let top = elaborate(&ast, HashSet::new(), "").expect("elaborate");
        assert_eq!(top.phrases.len(), 2);
        let Phrase::Run(comp) = &top.phrases[0].item else {
            panic!("expected a Run phrase, got {:?}", top.phrases[0].item);
        };
        let (name, _, _) = expect_exec_name(comp);
        assert_eq!(name, &CommandName::Bare("g".into()));
        let Phrase::Define {
            pattern, comp: rhs, ..
        } = &top.phrases[1].item
        else {
            panic!("expected a self-recursive Define, got {:?}", top.phrases[1].item);
        };
        assert!(matches!(pattern.as_ref(), IrPattern::Name(n) if n == "g"));
        let CompKind::Return(Val::Thunk(rec)) = &rhs.item else {
            panic!("expected Return(Thunk(Rec)), got {:?}", rhs.item);
        };
        assert!(
            matches!(rec.item, CompKind::Rec { index: 0, .. }),
            "expected the self-recursive binding to emit a Rec{{index: 0}}, got {:?}",
            rec.item
        );
    }

    /// The self-reference inside `f`'s body forces the forward-declared
    /// variable rather than shelling out to a command named `f`.
    #[test]
    fn intra_group_recursion_resolves_to_binding() {
        let ast = parse("let f = { |n| f $n }\nf 5").expect("parse");
        let top = elaborate(&ast, HashSet::new(), "").expect("elaborate");
        assert_eq!(top.phrases.len(), 2);
        let Phrase::Define { comp: rhs, .. } = &top.phrases[0].item else {
            panic!("expected a Define, got {:?}", top.phrases[0].item);
        };
        let CompKind::Return(Val::Thunk(rec)) = &rhs.item else {
            panic!("expected Return(Thunk(Rec)), got {:?}", rhs.item);
        };
        let CompKind::Rec { group, index } = &rec.item else {
            panic!("expected a Rec node, got {:?}", rec.item);
        };
        let (_, member) = &group[*index];
        let CompKind::Lam { body, .. } = &member.item else {
            panic!("expected a lambda RHS, got {:?}", member.item);
        };
        assert!(
            matches!(body.item, CompKind::App { .. }),
            "expected the self-reference to force the bound variable, got {:?}",
            body.item
        );
    }

    /// A `?`-chain arm is guarded, so the interpolation's `!{…}` hoist must
    /// live inside the arm: the statement elaborates to a bare `Chain`, never
    /// to a `Bind` that would run the hoist before the chain.
    #[test]
    fn chain_arm_hoist_stays_inside_the_arm() {
        let ast = parse(r#"return ok ? echo "fallback: !{hostname}""#).expect("parse");
        let comp = elaborate_one(&ast, HashSet::new(), "");
        assert!(
            matches!(comp.item, CompKind::Chain(_)),
            "chain arm hoist leaked into the caller: expected a bare Chain, got {:?}",
            comp.item
        );
    }

    /// `$SCRIPT` resolves to a string literal, not a runtime lookup.
    #[test]
    fn script_bakes_to_a_string_literal() {
        let ast = parse("return $SCRIPT").expect("parse");
        let comp = elaborate_one(&ast, HashSet::new(), "/repo/lib.ral");
        assert_eq!(
            comp.item,
            CompKind::Return(Val::String("/repo/lib.ral".into()))
        );
    }

    /// Sources with no script identity — the REPL, `-c`, a preloaded `<...>`
    /// source — reject `$SCRIPT` at elaboration time.
    #[test]
    fn script_with_no_identity_is_an_elaboration_error() {
        let ast = parse("return $SCRIPT").expect("parse");
        assert!(elaborate(&ast, HashSet::new(), "").is_err());
        assert!(elaborate(&ast, HashSet::new(), "-c").is_err());
        assert!(elaborate(&ast, HashSet::new(), "<stdin>").is_err());
    }

    // ── W1b: phrases and right-nested binders ────────────────────────────

    #[test]
    fn nested_sequence_is_wildcard_bind() {
        let stmts = parse("date\necho hi").expect("parse");
        let mut elaborator = Elaborator::new_with_bindings(HashSet::new(), "");
        let seq = elaborator.stmts_nested(&stmts);
        let CompKind::Bind {
            pattern,
            comp,
            rest,
            ..
        } = &seq.item
        else {
            panic!("expected a Bind, got {:?}", seq.item);
        };
        assert!(matches!(pattern.as_ref(), IrPattern::Wildcard));
        let (name, _, _) = expect_exec_name(comp);
        assert_eq!(name, &CommandName::Bare("date".into()));
        let (name, _, _) = expect_exec_name(rest);
        assert_eq!(name, &CommandName::Bare("echo".into()));
    }

    #[test]
    fn nested_let_wildcard_binds_a_fresh_name() {
        let stmts = parse("let _ = date\necho hi").expect("parse");
        let mut elaborator = Elaborator::new_with_bindings(HashSet::new(), "");
        let seq = elaborator.stmts_nested(&stmts);
        let CompKind::Bind { pattern, .. } = &seq.item else {
            panic!("expected a Bind, got {:?}", seq.item);
        };
        match pattern.as_ref() {
            IrPattern::Name(n) => assert!(n.starts_with("_var"), "not a gensym: {n}"),
            other => panic!("expected a fresh Name, never Wildcard, got {other:?}"),
        }
    }

    #[test]
    fn nested_let_right_nests_over_following_statements() {
        let stmts = parse("let x = 1\necho hi\necho bye").expect("parse");
        let mut elaborator = Elaborator::new_with_bindings(HashSet::new(), "");
        let seq = elaborator.stmts_nested(&stmts);
        let CompKind::Bind { pattern, rest, .. } = &seq.item else {
            panic!("expected a Bind, got {:?}", seq.item);
        };
        assert!(matches!(pattern.as_ref(), IrPattern::Name(n) if n == "x"));
        let CompKind::Bind {
            pattern: inner_pattern,
            rest: inner_rest,
            ..
        } = &rest.item
        else {
            panic!("expected the let to right-nest over echo hi, got {:?}", rest.item);
        };
        assert!(matches!(inner_pattern.as_ref(), IrPattern::Wildcard));
        let (name, _, _) = expect_exec_name(inner_rest);
        assert_eq!(name, &CommandName::Bare("echo".into()));
    }

    #[test]
    fn nested_block_ending_in_let_has_unit_tail() {
        let stmts = parse("let x = 1").expect("parse");
        let mut elaborator = Elaborator::new_with_bindings(HashSet::new(), "");
        let seq = elaborator.stmts_nested(&stmts);
        let CompKind::Bind { rest, .. } = &seq.item else {
            panic!("expected a Bind, got {:?}", seq.item);
        };
        assert_eq!(rest.item, CompKind::Return(Val::Unit));
    }

    #[test]
    fn source_tilde_path_hoists_observe_tilde() {
        let ast = parse("source ~/x.ral").expect("parse");
        let top = elaborate(&ast, HashSet::new(), "").expect("elaborate");
        assert_eq!(top.phrases.len(), 1);
        let Phrase::Source { path } = &top.phrases[0].item else {
            panic!("expected a Source phrase, got {:?}", top.phrases[0].item);
        };
        let CompKind::Bind { comp, .. } = &path.item else {
            panic!("expected a Bind over the hoisted temporary, got {:?}", path.item);
        };
        assert!(matches!(comp.item, CompKind::Observe(Register::Tilde(_))));
    }

    #[test]
    fn chained_source_is_not_a_source_phrase() {
        let ast = parse("source f ? fallback").expect("parse");
        let top = elaborate(&ast, HashSet::new(), "").expect("elaborate");
        assert_eq!(top.phrases.len(), 1);
        assert!(matches!(top.phrases[0].item, Phrase::Run(_)));
    }

    #[test]
    fn unbound_source_is_a_source_phrase() {
        let ast = parse("source x").expect("parse");
        let top = elaborate(&ast, HashSet::new(), "").expect("elaborate");
        assert_eq!(top.phrases.len(), 1);
        assert!(matches!(top.phrases[0].item, Phrase::Source { .. }));
    }

    #[test]
    fn bound_source_is_an_app_not_a_source_phrase() {
        let ast = parse("let source = { |x| return $x }\nsource x").expect("parse");
        let top = elaborate(&ast, HashSet::new(), "").expect("elaborate");
        assert_eq!(top.phrases.len(), 2);
        assert!(matches!(top.phrases[0].item, Phrase::Define { .. }));
        let Phrase::Run(comp) = &top.phrases[1].item else {
            panic!("expected a Run phrase, got {:?}", top.phrases[1].item);
        };
        assert!(matches!(comp.item, CompKind::App { .. }));
    }

    #[test]
    fn toplevel_phrases_classify_define_source_run() {
        let ast = parse("let x = 1\nsource cfg\necho hi").expect("parse");
        let top = elaborate(&ast, HashSet::new(), "").expect("elaborate");
        assert_eq!(top.phrases.len(), 3);
        assert!(matches!(top.phrases[0].item, Phrase::Define { .. }));
        assert!(matches!(top.phrases[1].item, Phrase::Source { .. }));
        assert!(matches!(top.phrases[2].item, Phrase::Run(_)));
    }

    #[test]
    fn toplevel_self_recursive_let_is_a_rec_define() {
        let ast = parse("let f = { |n| f $n }").expect("parse");
        let top = elaborate(&ast, HashSet::new(), "").expect("elaborate");
        assert_eq!(top.phrases.len(), 1);
        let Phrase::Define { pattern, comp, .. } = &top.phrases[0].item else {
            panic!("expected a Define, got {:?}", top.phrases[0].item);
        };
        assert!(matches!(pattern.as_ref(), IrPattern::Name(n) if n == "f"));
        let CompKind::Return(Val::Thunk(rec)) = &comp.item else {
            panic!("expected Return(Thunk(Rec)), got {:?}", comp.item);
        };
        let CompKind::Rec { group, index } = &rec.item else {
            panic!("expected a Rec node, got {:?}", rec.item);
        };
        assert_eq!(*index, 0);
        assert_eq!(group.len(), 1, "a self-recursive let is a group of one");
    }

    #[test]
    fn toplevel_mutual_recursion_shares_one_group_arc() {
        let ast = parse("let f = { |x| g $x }\nlet g = { |y| f $y }").expect("parse");
        let top = elaborate(&ast, HashSet::new(), "").expect("elaborate");
        assert_eq!(top.phrases.len(), 2);
        fn group_arc(phrase: &Phrase) -> Arc<[(String, Arc<Comp>)]> {
            let Phrase::Define { comp, .. } = phrase else {
                panic!("expected a Define, got {phrase:?}");
            };
            let CompKind::Return(Val::Thunk(rec)) = &comp.item else {
                panic!("expected Return(Thunk(Rec)), got {:?}", comp.item);
            };
            let CompKind::Rec { group, .. } = &rec.item else {
                panic!("expected a Rec node, got {:?}", rec.item);
            };
            group.clone()
        }
        let g1 = group_arc(&top.phrases[0].item);
        let g2 = group_arc(&top.phrases[1].item);
        assert!(Arc::ptr_eq(&g1, &g2), "both binders must share one group Arc");
        assert_eq!(g1[0].0, "f");
        assert_eq!(g1[1].0, "g");
    }

    #[test]
    fn cwd_reference_hoists_one_observe_cwd() {
        let ast = parse("echo $CWD").expect("parse");
        let top = elaborate(&ast, HashSet::new(), "").expect("elaborate");
        let Phrase::Run(comp) = &top.phrases[0].item else {
            panic!("expected a Run phrase, got {:?}", top.phrases[0].item);
        };
        let CompKind::Bind { comp: rhs, rest, .. } = &comp.item else {
            panic!("expected a Bind over the hoisted temporary, got {:?}", comp.item);
        };
        assert!(matches!(rhs.item, CompKind::Observe(Register::Cwd)));
        assert!(matches!(rest.item, CompKind::Exec(_)));
    }

    #[test]
    fn let_cwd_is_a_parse_error_naming_the_reading() {
        let ast = parse("let CWD = 1").expect("parse");
        let err = elaborate(&ast, HashSet::new(), "").expect_err("expected a ParseError");
        assert!(
            err.message.contains("working directory"),
            "message did not name the reading: {}",
            err.message
        );
    }
}
