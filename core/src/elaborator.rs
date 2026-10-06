//! Elaboration: surface AST into the call-by-push-value IR of [`crate::ir`].
//!
//! CBPV splits inert `Val` from effectful `Comp`.  Where the IR wants a `Val`
//! but the source has an effectful sub-expression, the elaborator binds it to
//! a fresh `%varN` and substitutes a `Val::Variable`; those pending bindings ride
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
    Args, Assembly, CaseArm, CommandName, CommandWord, Comp, CompKind, Exec, GroupNode,
    HandlerArmV, Name, OptionsV, Pattern, Phrase, Redirects, Unchecked, Val, ValListElem,
    ValMapEntry, ValRecordEntry, synthetic,
};
use crate::prelude_manifest;
use crate::source::Span;
use crate::source::Spanned;
use crate::source::WithSpan;
use crate::syntax::ast::{
    self, Ast, Head, IfBranch, ListElem, MapEntry, Options, RecordEntry, ScopeAst, Stmt, Word,
    WordLiteral,
};
use crate::syntax::group::{RecMember, StmtGroup, group_stmts};
use crate::syntax::parser::ParseError;
use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

/// State threaded through the elaboration pass.
struct Elaborator {
    /// Fresh-name counter for `gensym`.
    counter: usize,
    /// Prelude exports, in scope beneath every lexical frame.  Shared and never
    /// mutated, so an elaboration bumps a refcount rather than cloning the set.
    prelude: Arc<HashSet<String>>,
    /// Bound names, innermost last; the base frame holds the caller's bindings.
    lexical_scopes: Vec<HashSet<Name>>,
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
    fn new_with_bindings(bindings: impl IntoIterator<Item = Name>, name: &str) -> Self {
        Self {
            counter: 0,
            prelude: prelude_scope(),
            lexical_scopes: vec![bindings.into_iter().collect()],
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
            return Val::Variable(name.into());
        }
        if let Some(s) = &self.script {
            return Val::String(s.clone().into());
        }
        self.refuse(
            self.current_span,
            "$SCRIPT: no script name here (the REPL, `-c`, and preloaded sources have none)",
        );
        Val::Unit
    }

    /// A name for a hoisted temporary.
    fn gensym(&mut self) -> Name {
        self.counter += 1;
        synthetic("var", self.counter)
    }

    /// Elaboration's one failure: the first refusal stands.
    fn refuse(&mut self, span: Option<Span>, message: &str) {
        self.error
            .get_or_insert_with(|| ParseError::new(span, message));
    }

    /// The one door into scope, hence the one place `SCRIPT` is refused as
    /// a binder; `span` is the binder's own.
    fn bind(&mut self, span: Option<Span>, names: impl IntoIterator<Item = Name>) {
        for name in names {
            if &*name == "SCRIPT" {
                self.refuse(
                    span,
                    "$SCRIPT names the file being compiled, not a name you can bind; choose \
                     another name",
                );
            }
            self.lexical_scopes
                .last_mut()
                .expect("lexical_scopes is initialised non-empty and never popped past 1")
                .insert(name);
        }
    }

    fn bind_pattern(&mut self, pat: &Spanned<Pattern>) {
        self.bind(pat.span, pat.item.names().into_iter().cloned());
    }

    /// A `{ |param| body }` binder together with the statements it scopes:
    /// the body elaborates inside the frame the param's names open.  Both
    /// readings of that spelling — the lambda it denotes and the `case` arm
    /// that is a branch rather than a function — get their scope from here,
    /// so it is stated once.
    fn elab_binder_scope(&mut self, param: &Spanned<Pattern>, body: &[Stmt]) -> (Pattern, Comp) {
        let body = self.with_new_scope(|this| {
            this.bind_pattern(param);
            this.stmts_nested(body)
        });
        (param.item.clone(), body)
    }

    /// A fresh frame, so the body's `let`s shadow rather than leak outward.
    fn with_new_scope<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved_span = self.current_span;
        self.lexical_scopes.push(HashSet::new());
        let out = f(self);
        self.lexical_scopes.pop();
        self.current_span = saved_span;
        out
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
    fn exec(&self, head: CommandWord, args: Args, redirects: Redirects<Val>) -> Comp {
        comp!(
            self,
            CompKind::Exec(Exec {
                head,
                args,
                redirects,
                site: None,
            })
        )
    }

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

    /// Forward-declares a recursive knot's own names, then elaborates each
    /// member's RHS to its thunk body, sharing one `group` `Arc` — Levy's
    /// `rec x⃗. M⃗`.  Confining the forward declaration to the group, rather
    /// than scanning ahead over earlier statements, keeps a preceding
    /// command use of the same name lowering to `Exec` instead of a
    /// dangling `Force(Variable)`.
    fn build_rec_group(&mut self, members: &[RecMember]) -> Arc<GroupNode> {
        for RecMember { name, .. } in members {
            self.bind(name.span, [name.item.as_str().into()]);
        }
        let members: Vec<(Name, Arc<Comp>)> = members
            .iter()
            .map(|RecMember { name, value }| {
                let mut empty = Vec::new();
                let CompKind::Return(Val::Thunk(node)) = self
                    .with_span(value.span, |this| this.elab_expr(&value.item, &mut empty))
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
                (name.item.as_str().into(), Arc::clone(node.shape()))
            })
            .collect();
        GroupNode::new(members.into())
    }

    /// One top-level phrase (depth 0): a `let` becomes a `Define`,
    /// anything else a `Run`.  The `Define` pattern's names enter scope only
    /// after the RHS is elaborated, as `nested_single`'s `let` arm does.
    fn toplevel_phrase(&mut self, stmt: Stmt) -> Spanned<Phrase<()>> {
        let Spanned { item: kind, span } = stmt;
        self.with_span(span, |this| {
            if let Ast::Let { pattern, value } = &kind {
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
            let comp = this.elab_expr(&kind, &mut binds);
            let comp = wrap_binds(this.current_span, binds, comp);
            Spanned::with_span(span, Phrase::Run(Arc::new(comp)))
        })
    }

    /// A recursive knot at depth 0: *n* `Define`s, `xᵢ = Return(Thunk(Rec{group,
    /// i}))`, sharing one `group` `Arc`.
    fn toplevel_rec_group(&mut self, members: &[RecMember]) -> Unchecked {
        let group = self.build_rec_group(members);
        members
            .iter()
            .enumerate()
            .map(|(index, RecMember { name, value })| {
                let span = value.span;
                let rec = Spanned::with_span(
                    span,
                    CompKind::Rec {
                        group: Arc::clone(&group),
                        index,
                    },
                );
                let comp = Arc::new(Spanned::with_span(
                    span,
                    CompKind::Return(Val::thunk(Arc::new(rec))),
                ));
                Spanned::with_span(
                    span,
                    Phrase::Define {
                        pattern: Arc::new(Pattern::Name(name.item.as_str().into())),
                        comp,
                        schemes: (),
                    },
                )
            })
            .collect()
    }

    /// Elaborate `stmts` at depth > 0 — every block or lambda body — right-
    /// nesting each statement into a `Bind` chain over what follows it, so
    /// `{ a; b }` is `a to _. b`.  The tail of the block is its last
    /// statement's own comp, not a `Bind` on it; a block ending in a `let`
    /// has tail `Return(Unit)`.
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
            rest = unit.into_bind(span, rest);
        }
        rest
    }

    /// One statement at depth > 0: a `let` binds a fresh `Name` (never
    /// `Wildcard` — a surface `_` gets the hygienic gensym `$[…]` temporaries
    /// already use), and anything else is a discard, marked `Wildcard`,
    /// which no surface pattern ever produces.
    fn nested_single(&mut self, stmt: Stmt) -> (Option<Span>, NestedUnit) {
        let Spanned { item: kind, span } = stmt;
        self.with_span(span, |this| {
            if let Ast::Let { pattern, value } = &kind {
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
            let comp = this.stmt(&kind);
            (span, NestedUnit::Other { comp })
        })
    }

    /// A recursive knot at depth > 0: *n* nested `Bind`s over `Return(Thunk(
    /// Rec{group, i}))`, sharing one `group` `Arc`, in source order.
    fn nested_rec_group(&mut self, members: &[RecMember]) -> Vec<(Option<Span>, NestedUnit)> {
        let group = self.build_rec_group(members);
        members
            .iter()
            .enumerate()
            .map(|(index, RecMember { name, value })| {
                let span = value.span;
                let rec = Spanned::with_span(
                    span,
                    CompKind::Rec {
                        group: Arc::clone(&group),
                        index,
                    },
                );
                let rhs = Spanned::with_span(span, CompKind::Return(Val::thunk(Arc::new(rec))));
                (
                    span,
                    NestedUnit::Bind {
                        rhs,
                        pattern: Pattern::Name(name.item.as_str().into()),
                    },
                )
            })
            .collect()
    }

    /// Elaborate `ast` as a computation, pushing every sub-expression that must
    /// run before its parent into `binds` for the caller to `wrap_binds`.
    fn elab_expr(&mut self, ast: &Ast, binds: &mut Vec<(Pattern, Comp)>) -> Comp {
        match ast {
            Ast::Word(Word::Plain(s) | Word::Slash(s)) => {
                comp!(self, CompKind::Return(word_val(s)))
            }
            Ast::Literal(s) => comp!(self, CompKind::Return(Val::String(s.clone().into()))),
            Ast::Variable(s) => {
                comp!(self, CompKind::Return(self.variable_val(s)))
            }
            Ast::Word(Word::Tilde(path)) => {
                let tilde = comp!(self, CompKind::Tilde(path.clone()));
                let v = self.hoist(tilde, binds);
                comp!(self, CompKind::Return(v))
            }

            Ast::Block(body) => {
                let body_comp = self.with_new_scope(|this| this.stmts_nested(body));
                comp!(self, CompKind::Return(Val::thunk(Arc::new(body_comp))))
            }

            Ast::Lambda { param, body } => {
                let (param_ir, body_comp) = self.elab_binder_scope(param, body);
                // A body that is itself a single lambda already carries its own
                // thunk; reuse it rather than wrapping a thunk in a thunk.
                let body_arc: Arc<Comp> = match &body_comp.item {
                    CompKind::Return(Val::Thunk(inner))
                        if matches!(inner.shape().item, CompKind::Lam { .. }) =>
                    {
                        Arc::clone(inner.shape())
                    }
                    _ => Arc::new(body_comp),
                };
                comp!(
                    self,
                    CompKind::Return(Val::thunk(Arc::new(comp!(
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
                    Some(self.elab_expr(value, binds))
                } else {
                    None
                };

                // Only an argument-position spread splices here: a list-literal
                // argument `f [...$xs]` stays one arg carrying its own spread.
                let arg_vals: Args = args
                    .iter()
                    .map(|a| match &a.item {
                        Ast::Spread(inner) => ValListElem::Spread(Spanned::with_span(
                            a.span,
                            self.with_span(a.span, |this| this.to_val(&inner.item, binds)),
                        )),
                        other => ValListElem::Single(Spanned::with_span(
                            a.span,
                            self.with_span(a.span, |this| this.to_val(other, binds)),
                        )),
                    })
                    .collect();
                let redirect_vals = self.lower_redirects(redirects, binds);

                match head {
                    Head::ExternalName(s) => self.exec(
                        CommandWord::External(CommandName::Bare(s.clone().into())),
                        arg_vals,
                        redirect_vals,
                    ),
                    Head::Bare(s) if self.is_bound(s) => {
                        // The `Force` is what makes the `Force` rule run a
                        // bound block inside the redirect frame wrapped
                        // around it.
                        let head_comp =
                            comp!(self, CompKind::Force(Val::Variable(s.clone().into())));
                        self.apply_head(head_comp, arg_vals, redirect_vals)
                    }
                    Head::Bare(s) => self.exec(
                        CommandWord::Name(CommandName::Bare(s.clone().into())),
                        desugar_zero_arg_exit(s, arg_vals),
                        redirect_vals,
                    ),
                    Head::Path(path) => self.exec(
                        CommandWord::Name(CommandName::Path(path.clone())),
                        arg_vals,
                        redirect_vals,
                    ),
                    Head::TildePath(path) => self.exec(
                        CommandWord::Name(CommandName::TildePath(path.clone())),
                        arg_vals,
                        redirect_vals,
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
                    ScopeAst::Within {
                        opts,
                        handlers,
                        body,
                    } => {
                        let opts = self.lower_options(opts, binds);
                        let handlers = handlers.as_ref().map(|arms| {
                            arms.iter()
                                .map(|arm| HandlerArmV {
                                    name: arm.name.clone(),
                                    value: Spanned::with_span(
                                        arm.value.span,
                                        self.to_val(&arm.value.item, binds),
                                    ),
                                })
                                .collect()
                        });
                        comp!(
                            self,
                            CompKind::Within {
                                opts,
                                handlers,
                                body: self.to_val(body, binds),
                            }
                        )
                    }
                    ScopeAst::Grant { caps, body } => comp!(
                        self,
                        CompKind::Grant {
                            caps: self.lower_options(caps, binds),
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
                comp!(self, CompKind::Pipeline { stages: comps })
            }

            Ast::Chain(parts) => {
                // `a ? b ? c` is `try a { |_| try b { |_| c } }`, right-nested
                // so the last arm stays in tail position.  Every arm but the
                // first runs only when its predecessors failed, so each needs
                // the fresh binds vector `elab_guarded` gives it.
                let mut arms: Vec<(Option<Span>, Comp)> = parts
                    .iter()
                    .map(|a| {
                        (
                            a.span,
                            self.with_span(a.span, |this| this.elab_guarded(&a.item)),
                        )
                    })
                    .collect();
                match arms.pop() {
                    None => comp!(self, CompKind::Return(Val::Unit)),
                    Some((_, last)) => {
                        arms.into_iter()
                            .rev()
                            .fold(last, |handler_body, (span, arm)| {
                                self.with_span(span, |this| {
                                    let handler = Val::thunk(Arc::new(comp!(
                                        this,
                                        CompKind::Lam {
                                            param: Pattern::Wildcard,
                                            body: Arc::new(handler_body),
                                        }
                                    )));
                                    comp!(
                                        this,
                                        CompKind::Try {
                                            body: Val::thunk(Arc::new(arm)),
                                            handler,
                                        }
                                    )
                                })
                            })
                    }
                }
            }

            Ast::List(elems) => {
                let plain: Option<Vec<&Spanned<Ast>>> = elems
                    .iter()
                    .map(|e| match e {
                        ListElem::Single(a) => Some(a),
                        ListElem::Spread(_) => None,
                    })
                    .collect();
                match plain {
                    Some(items) => comp!(
                        self,
                        CompKind::Return(Val::list(
                            items
                                .into_iter()
                                .map(|a| self.spanned_val(a, binds))
                                .collect::<Vec<_>>(),
                        ))
                    ),
                    None => comp!(
                        self,
                        CompKind::Assemble(Assembly::List(
                            elems
                                .iter()
                                .map(|e| match e {
                                    ListElem::Single(a) => {
                                        ValListElem::Single(self.spanned_val(a, binds))
                                    }
                                    ListElem::Spread(a) => {
                                        ValListElem::Spread(self.spanned_val(a, binds))
                                    }
                                })
                                .collect(),
                        ))
                    ),
                }
            }

            Ast::Record(entries) => {
                let plain: Option<Vec<(&String, &Spanned<Ast>)>> = entries
                    .iter()
                    .map(|e| match e {
                        RecordEntry::Field { key, value } => Some((key, value)),
                        RecordEntry::Spread(_) => None,
                    })
                    .collect();
                match plain {
                    Some(items) => {
                        let fields = self.sorted_fields(items, binds);
                        comp!(self, CompKind::Return(Val::record(fields)))
                    }
                    None => comp!(
                        self,
                        CompKind::Assemble(Assembly::Record(
                            entries
                                .iter()
                                .map(|e| match e {
                                    RecordEntry::Field { key, value } => ValRecordEntry::Field(
                                        key.clone(),
                                        self.spanned_val(value, binds),
                                    ),
                                    RecordEntry::Spread(a) => {
                                        ValRecordEntry::Spread(self.spanned_val(a, binds))
                                    }
                                })
                                .collect(),
                        ))
                    ),
                }
            }

            Ast::Map(entries) => {
                let plain: Option<Vec<(&String, &Spanned<Ast>)>> = entries
                    .iter()
                    .map(|e| match e {
                        MapEntry::Entry { key, value } => Some((key, value)),
                        MapEntry::Deref { .. } | MapEntry::Spread(_) => None,
                    })
                    .collect();
                match plain {
                    Some(items) => {
                        let fields = self.sorted_fields(items, binds);
                        comp!(self, CompKind::Return(Val::map(fields)))
                    }
                    None => comp!(
                        self,
                        CompKind::Assemble(Assembly::Map(
                            entries
                                .iter()
                                .map(|e| match e {
                                    MapEntry::Entry { key, value } => ValMapEntry::Entry(
                                        Val::String(key.clone().into()),
                                        self.spanned_val(value, binds),
                                    ),
                                    MapEntry::Deref { name, value } => {
                                        let key = self.variable_val(name);
                                        ValMapEntry::Entry(key, self.spanned_val(value, binds))
                                    }
                                    MapEntry::Spread(a) => {
                                        ValMapEntry::Spread(self.spanned_val(a, binds))
                                    }
                                })
                                .collect(),
                        ))
                    ),
                }
            }

            Ast::Tag { label, payload } => {
                let payload_val = payload
                    .as_ref()
                    .map(|p| self.with_span(p.span, |this| Box::new(this.to_val(&p.item, binds))));
                comp!(
                    self,
                    CompKind::Return(Val::Variant {
                        label: label.as_str().into(),
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

            Ast::Binary(l, op, r) => {
                let lv = self.with_span(l.span, |this| this.to_val(&l.item, binds));
                let rv = self.with_span(r.span, |this| this.to_val(&r.item, binds));
                comp!(self, CompKind::Binary(*op, lv, rv))
            }
            Ast::Negate(inner) => {
                let v = self.with_span(inner.span, |this| this.to_val(&inner.item, binds));
                comp!(self, CompKind::Negate(v))
            }
            Ast::Not(inner) => {
                let v = self.with_span(inner.span, |this| this.to_val(&inner.item, binds));
                comp!(self, CompKind::Not(v))
            }
            Ast::And(l, r) => self.lower_short_circuit(l, r, binds, Junction::And),
            Ast::Or(l, r) => self.lower_short_circuit(l, r, binds, Junction::Or),

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
    /// local vector wrapped inside the thunk that guards it.
    fn elab_if(
        &mut self,
        branches: &[IfBranch],
        else_: Option<&Spanned<Box<Ast>>>,
        binds: &mut Vec<(Pattern, Comp)>,
    ) -> Comp {
        let (first, rest) = branches
            .split_first()
            .expect("if must have at least one branch");
        let one_armed = rest.is_empty() && else_.is_none();

        let mut else_arm = match else_ {
            Some(e) => self.elab_arm(e),
            None => Self::thunk_of(comp!(self, CompKind::Return(Val::Unit))),
        };
        for branch in rest.iter().rev() {
            let mut local_binds = Vec::new();
            let cond_val = self.to_val(&branch.cond.item, &mut local_binds);
            let then_arm = self.elab_arm(&branch.body);
            let nested = comp!(
                self,
                CompKind::If {
                    cond: Spanned::with_span(branch.cond.span, cond_val),
                    then: then_arm,
                    else_: else_arm,
                }
            );
            else_arm = Self::thunk_of(wrap_binds(self.current_span, local_binds, nested));
        }

        let cond_val = self.to_val(&first.cond.item, binds);
        let then_arm = if one_armed {
            self.elab_arm_unit(&first.body)
        } else {
            self.elab_arm(&first.body)
        };
        comp!(
            self,
            CompKind::If {
                cond: Spanned::with_span(first.cond.span, cond_val),
                then: then_arm,
                else_: else_arm,
            }
        )
    }

    /// `comp` as the literal thunk a branching form forces.
    fn thunk_of(comp: Comp) -> Spanned<Val> {
        Spanned::with_span(comp.span, Val::thunk(Arc::new(comp)))
    }

    /// One branch of an `if` or a `case`: a literal `{ … }` or `{ |p| … }`, or
    /// a name in hand — the thunk the form forces.  Anything else would be
    /// hoisted, and so run before the form chose.
    fn elab_arm(&mut self, arm: &Spanned<Box<Ast>>) -> Spanned<Val> {
        let mut hoisted = Vec::new();
        let comp = self.with_span(arm.span, |this| this.elab_expr(&arm.item, &mut hoisted));
        let val = match comp.item {
            CompKind::Return(val) if hoisted.is_empty() => val,
            _ => {
                self.refuse(
                    arm.span.or(self.current_span),
                    "an arm is a block, or a name holding one; to compute the block first, \
                     bind it: `let arm = $arms[a]`",
                );
                Val::Unit
            }
        };
        Spanned::with_span(arm.span, val)
    }

    /// [`Self::elab_arm`] for the lone arm of an `else`-less `if`, whose result
    /// is discarded so that the form types as `F Unit`: `{ body; () }`, forcing
    /// a thunk in hand under the wrapper.  A value that is no block is left
    /// for the checker to refuse.
    fn elab_arm_unit(&mut self, arm: &Spanned<Box<Ast>>) -> Spanned<Val> {
        let arm = self.elab_arm(arm);
        let body = match &arm.item {
            Val::Thunk(node) => Arc::clone(node.shape()),
            Val::Variable(_) => Arc::new(comp!(self, CompKind::Force(arm.item.clone()))),
            _ => return arm,
        };
        Self::thunk_of(comp!(
            self,
            CompKind::bind(
                Pattern::Wildcard,
                body,
                comp!(self, CompKind::Return(Val::Unit))
            )
        ))
    }

    fn elab_case_arm(&mut self, arm: &ast::CaseArm) -> CaseArm {
        CaseArm {
            tag: arm.tag.clone(),
            body: self.elab_arm(&arm.body),
        }
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

    /// Yield the `Val` the parent consumes: `Return(v)` passes through, and
    /// anything else is bound to a fresh `%varN` pushed onto `binds`.
    fn hoist(&mut self, comp: Comp, binds: &mut Vec<(Pattern, Comp)>) -> Val {
        if let CompKind::Return(v) = comp.item {
            v
        } else {
            let name = self.gensym();
            binds.push((Pattern::Name(name.clone()), comp));
            Val::Variable(name)
        }
    }

    #[allow(clippy::wrong_self_convention)]
    fn to_val(&mut self, ast: &Ast, binds: &mut Vec<(Pattern, Comp)>) -> Val {
        let comp = self.elab_expr(ast, binds);
        self.hoist(comp, binds)
    }

    /// [`Self::to_val`], spanned at `ast`'s own range — every literal
    /// element and entry value.
    fn spanned_val(
        &mut self,
        ast: &Spanned<Ast>,
        binds: &mut Vec<(Pattern, Comp)>,
    ) -> Spanned<Val> {
        Spanned::with_span(
            ast.span,
            self.with_span(ast.span, |this| this.to_val(&ast.item, binds)),
        )
    }

    /// A plain record or map literal's entries, elaborated in written order and
    /// then sorted by key, stably.
    fn sorted_fields(
        &mut self,
        items: Vec<(&String, &Spanned<Ast>)>,
        binds: &mut Vec<(Pattern, Comp)>,
    ) -> Vec<(Name, Spanned<Val>)> {
        let mut fields: Vec<(Name, _)> = items
            .into_iter()
            .map(|(key, value)| (key.as_str().into(), self.spanned_val(value, binds)))
            .collect();
        fields.sort_by(|(a, _), (b, _)| a.cmp(b));
        fields
    }

    /// Shared by the two value-application heads, a bound bare name (`f x`) and
    /// an explicit value head (`$f x`, `{…} x`).  A zero-arg call is the head
    /// computation alone: `App` with an empty argument list is not a CBPV form.
    fn apply_head(&self, head_comp: Comp, arg_vals: Args, redirects: Redirects<Val>) -> Comp {
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
    /// `Exec` carries its own redirects instead, and pipelines and chains take
    /// none at the surface, so this covers every remaining body.
    fn wrap_redirect(&self, body: Comp, redirects: Redirects<Val>) -> Comp {
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

    /// Lower each redirect's operand, hoisting an effectful one into `binds`
    /// like any other value.
    fn lower_redirects(
        &mut self,
        redirects: &Redirects<Ast>,
        binds: &mut Vec<(Pattern, Comp)>,
    ) -> Redirects<Val> {
        redirects.map(|a| self.to_val(a, binds))
    }

    /// A form's written options, each value hoisted like any other value.
    fn lower_options(&mut self, opts: &Options, binds: &mut Vec<(Pattern, Comp)>) -> OptionsV {
        opts.iter()
            .map(|(name, value)| (Name::from(name.as_str()), self.spanned_val(value, binds)))
            .collect()
    }

    /// Desugar `a && b` / `a || b` into an `If`.  The RHS runs only
    /// conditionally, so it lowers in an isolated `binds` vector.
    fn lower_short_circuit(
        &mut self,
        l: &Spanned<Box<Ast>>,
        r: &Spanned<Box<Ast>>,
        binds: &mut Vec<(Pattern, Comp)>,
        junction: Junction,
    ) -> Comp {
        let cond = self.with_span(l.span, |this| this.to_val(&l.item, binds));
        let mut r_binds = Vec::new();
        let r_comp = self.with_span(r.span, |this| this.elab_expr(&r.item, &mut r_binds));
        let r_comp = wrap_binds(self.current_span, r_binds, r_comp);
        let rhs = Self::thunk_of(r_comp);
        let short = Self::thunk_of(comp!(
            self,
            CompKind::Return(Val::Bool(junction == Junction::Or))
        ));
        let (then_arm, else_arm) = match junction {
            Junction::And => (rhs, short),
            Junction::Or => (short, rhs),
        };
        comp!(
            self,
            CompKind::If {
                // No surface token holds this cond, so it carries no span and
                // diagnostics fall back to the enclosing one.
                cond: Spanned::synthetic(cond),
                then: then_arm,
                else_: else_arm,
            }
        )
    }
}

/// `&&` runs its right side when the left is true, `||` when it is false.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Junction {
    And,
    Or,
}

/// Fold `binds` into a chain of `Comp::Bind` nodes around `inner`, the first
/// binding outermost so the chain runs in the order the hoists were pushed.
fn wrap_binds(span: Option<Span>, binds: Vec<(Pattern, Comp)>, inner: Comp) -> Comp {
    binds
        .into_iter()
        .rev()
        .fold(inner, |rest, (pattern, comp)| {
            Spanned::with_span(span, CompKind::bind(pattern, comp, rest))
        })
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

/// Sugar bare `exit` / `quit` into `exit 0` / `quit 0`.  The builtin tolerates
/// zero args, but ral's fixed-arity rule gives it one `Int` slot; supplying the
/// status here spares the typechecker a zero-arg special case.
fn desugar_zero_arg_exit(name: &str, args: Args) -> Args {
    if args.is_empty() && (name == "exit" || name == "quit") {
        Args::from(vec![ValListElem::Single(Spanned::synthetic(Val::Int(0)))])
    } else {
        args
    }
}

/// The prelude's exported names, built once and shared by refcount thereafter.
fn prelude_scope() -> Arc<HashSet<String>> {
    static PRELUDE: LazyLock<Arc<HashSet<String>>> = LazyLock::new(|| {
        Arc::new(
            prelude_manifest::PRELUDE_EXPORTS
                .iter()
                .map(ToString::to_string)
                .collect(),
        )
    });
    Arc::clone(&PRELUDE)
}

/// The value a bare word denotes, by the shape rules of
/// [`WordLiteral::classify`].
///
/// Eager and type-blind: a numeric-looking word meant as argv data is read as
/// a number, and stringifies back unchanged only where its source was already
/// canonical (`007` ⇒ `7`, `1.50` ⇒ `1.5`).
pub(crate) fn word_val(s: &str) -> Val {
    match WordLiteral::classify(s) {
        Some(WordLiteral::Bool(b)) => Val::Bool(b),
        Some(WordLiteral::Int(n)) => Val::Int(n),
        Some(WordLiteral::Float(f)) => Val::Float(f),
        None => Val::String(s.into()),
    }
}

/// Elaborate a top-level statement sequence into unchecked phrases.
///
/// Each `let` becomes a `Define`, a `let`-knot becomes one `Define` per
/// member sharing a `Rec` group, and everything else a `Run`.
///
/// `bindings` are the names already live in the calling environment (a REPL's
/// accumulated definitions, say); the prelude is always in scope.  `name` is the
/// source's own display name, the value `$SCRIPT` resolves to.
///
/// # Errors
/// `$SCRIPT` referenced where `name` carries no script identity, or bound by
/// a pattern.
pub fn elaborate(
    ast: &[Stmt],
    bindings: impl IntoIterator<Item = Name>,
    name: &str,
) -> Result<Unchecked, ParseError> {
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
    Ok(phrases)
}

#[cfg(test)]
mod tests;
