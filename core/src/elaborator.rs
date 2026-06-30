//! Elaboration: translate the surface AST into the CBPV intermediate
//! representation (`Comp`/`Val` from [`crate::ir`]).
//!
//! # CBPV in one paragraph
//!
//! The IR follows a *call-by-push-value* (CBPV) discipline.  Values (`Val`)
//! are inert data — strings, lists, maps, thunks — that require no evaluation.
//! Computations (`Comp`) are effectful and sequenced.  The elaborator enforces
//! this split: wherever the IR requires a `Val` but the source has an
//! effectful sub-expression, the sub-expression is bound to a fresh temporary
//! and replaced by a `Val::Variable`.  This is done by threading a mutable
//! *binds* accumulator (`Vec<(IrPattern, Comp)>`) through `elab_expr`; callers
//! call `wrap_binds` at statement boundaries to fold the accumulated bindings
//! into a chain of `Comp::Bind` nodes.
//!
//! # Lexical scope
//!
//! The elaborator tracks which names are in scope (via `Elaborator::lexical_scopes`)
//! to decide how to elaborate a command head:
//!
//! - A bare name in scope → `Comp::App(Force(Variable(name)), …)` — the value
//!   bound to that name is retrieved and called.
//! - An unbound bare name → `Comp::Exec { name, … }` — the name is treated as
//!   an external command looked up via the command namespace.
//! - An external-name head (`^name`) → `Comp::Exec { head: CommandWord::External(name), … }`
//!   — value/alias/builtin lookup is skipped and PATH is used directly.
//! - A literal path head (`./x`, `/x`) or tilde-path head (`~/x`) →
//!   `Comp::Exec { Path/TildePath, … }` — the exact path is executed at the
//!   process boundary.
//! - Any other explicit value head (`$f`, `!$f`, `{ |x| ... }`, etc.) is
//!   elaborated as an ordinary value application and never performs external
//!   command lookup.
//!
//! The prelude's exports are pre-loaded into the outermost scope, and any
//! names already bound in the calling environment (e.g. REPL bindings) are
//! passed in at construction time.
//!
//! # Entry point
//!
//! [`elaborate`] is the only public function.  It calls the [`group`] pre-pass
//! to detect mutually recursive binding groups, then elaborates each group.

use crate::ir::*;
use crate::prelude_manifest;
use crate::source::Span;
use crate::source::Spanned;
use crate::source::WithSpan;
use crate::syntax::ast::*;
use crate::syntax::group::{StmtGroup, group_stmts};
use std::collections::HashSet;
use std::sync::Arc;

/// State threaded through the elaboration pass.
///
/// Tracks fresh-name generation, lexical scopes for bare-name resolution,
/// and the most recently seen source span for attaching to emitted IR.
struct Elaborator {
    /// Counter for generating fresh variable names (`_g1`, `_g2`, …) when
    /// hoisting effectful sub-expressions into `Comp::Bind` nodes.
    counter: usize,
    /// Stack of lexical scopes.  Each scope is a set of bound names.  The
    /// outermost scope holds the prelude exports; inner scopes are pushed for
    /// lambda bodies, blocks, and `let` groups.
    lexical_scopes: Vec<HashSet<String>>,
    /// The most recently seen source span — attached to every emitted `Comp`.
    /// Narrowed scopewise by [`Self::with_span`]; every traversal that knows
    /// a narrower byte range than its caller wraps the body in `with_span`,
    /// so the prior span is restored when the body returns.
    current_span: Option<Span>,
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
    /// Create an elaborator whose initial scope contains the prelude
    /// exports and the given `bindings` (e.g. names already defined in
    /// a REPL session).
    fn new_with_bindings(bindings: HashSet<String>) -> Self {
        Elaborator {
            counter: 0,
            lexical_scopes: vec![prelude_scope(), bindings],
            current_span: None,
        }
    }

    /// Generate a fresh variable name (`_g1`, `_g2`, ...) for hoisted binds.
    fn gensym(&mut self) -> String {
        self.counter += 1;
        format!("_g{}", self.counter)
    }

    /// Mutable handle to the innermost lexical scope.  `lexical_scopes`
    /// is initialised with the prelude and the caller's bindings frame
    /// (see `new_with_bindings`) and only grows from there via
    /// [`with_bound_names`] / [`with_new_scope`], so this is total —
    /// no caller observes an empty scope stack.
    fn current_scope_mut(&mut self) -> &mut HashSet<String> {
        self.lexical_scopes
            .last_mut()
            .expect("lexical_scopes is initialised non-empty and never popped past 1")
    }

    /// Record all names introduced by `pat` in the current scope.
    fn bind_pattern(&mut self, pat: &Pattern) {
        pat.collect_names(self.current_scope_mut());
    }

    /// Translate an AST pattern into an [`IrPattern`], elaborating each
    /// map-pattern default at the current lexical context.  Defaults are
    /// elaborated *before* the pattern's own names enter scope, so a
    /// default like `[host: h = $h]` resolves `$h` against the outer
    /// environment rather than (cyclically) the pattern binding.
    fn elab_pattern(&mut self, pat: &Pattern) -> IrPattern {
        match pat {
            Pattern::Wildcard => IrPattern::Wildcard,
            Pattern::Name(n) => IrPattern::Name(n.clone()),
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
                            // Defaults are statement-shaped: elaborate via
                            // `stmts` so a bare command head, a value, or a
                            // chain all work identically.  The result is a
                            // single `Comp` that produces the default value
                            // when run.  Wrap the surface `Ast` in a
                            // synthetic `Stmt` carrying the elaborator's
                            // current span — defaults have no source span
                            // of their own; the enclosing pattern's span
                            // is the natural fallback.
                            let stmt = [Spanned::with_span(self.current_span, d.clone())];
                            Arc::new(self.stmts(&stmt))
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

    /// Push a fresh scope containing `names`, run `f`, then pop the scope.
    /// Also saves and restores `current_span` so that inner elaboration
    /// does not leak span state outward.
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

    /// Push a fresh empty scope, run `f`, then pop.  Used for block
    /// bodies and `if`-branch bodies where there are no parameter
    /// names to introduce — the scope still matters because nested
    /// `let` bindings should shadow at the block, not leak outward.
    fn with_new_scope<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.with_bound_names(std::iter::empty::<String>(), f)
    }

    /// True if `name` is bound in any enclosing scope (searched innermost first).
    fn is_bound(&self, name: &str) -> bool {
        self.lexical_scopes
            .iter()
            .rev()
            .any(|scope| scope.contains(name))
    }

    /// Build an `Exec` computation at the current span.  All name-dispatched
    /// command heads (`bare`, `^name`, `./path`, `~/path`) funnel through here.
    fn exec(
        &mut self,
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

    /// Elaborate a statement sequence into a single `Comp`.
    /// An empty sequence returns `Comp::Return(Val::Unit)`.
    /// A single-element sequence returns that element's `Comp` unwrapped.
    /// Multiple elements are wrapped in `Comp::Seq`.
    fn stmts(&mut self, stmts: &[Stmt]) -> Comp {
        // `group_stmts` walks the statements in source order, emitting each
        // recursive SCC as a `LetRec` group at its head's position and every
        // other statement as a `Single`.  Forward-declaration of the
        // recursive names is scoped to the `LetRec` arm of `emit_group` (it
        // inserts them just before elaborating their own RHS bodies), so a
        // source-preceding command use of the same name is emitted as a
        // `Single` first and correctly lowers to `Exec` rather than a
        // dangling `Force(Variable)`.
        let groups = group_stmts(stmts);
        let comps: Vec<Comp> = groups.into_iter().map(|g| self.emit_group(g)).collect();
        match comps.len() {
            0 => comp!(self, CompKind::Return(Val::Unit)),
            1 => comps.into_iter().next().unwrap(),
            _ => comp!(
                self,
                CompKind::Seq(comps.into_iter().map(Arc::new).collect())
            ),
        }
    }

    /// Elaborate a single statement group (produced by the [`group`] pre-pass).
    ///
    /// For a single statement, stamps its span onto `current_span` (so
    /// the emitted IR — and any unification failures it provokes —
    /// underline the statement's source location) and then elaborates
    /// the underlying kind.  `LetRec` groups elaborate each binding
    /// body (unwrapping `Return(Thunk(…))` to expose the inner lambda)
    /// and emit a `CompKind::LetRec` node.
    fn emit_group(&mut self, group: StmtGroup) -> Comp {
        match group {
            StmtGroup::Single(stmt) => {
                // Stamp the statement's span via `with_span` so it
                // auto-restores when the arm exits — siblings in the
                // surrounding `stmts` loop start from a clean span.
                self.with_span(stmt.span, |this| {
                    let Spanned { item: kind, .. } = stmt;
                    let comp = this.stmt(&kind);
                    if let Ast::Let { pattern, .. } = &kind {
                        this.bind_pattern(&pattern.item);
                    }
                    comp
                })
            }
            StmtGroup::LetRec(bindings) => {
                // Forward-declare the group's own names before elaborating any
                // RHS, so a sibling reference inside the group resolves to a
                // variable.  Confining the declaration to the recursive group
                // (rather than scanning ahead over preceding statements) keeps
                // an earlier command use of the same name lowering to `Exec`.
                // Each RHS becomes a thunk value — mutual recursion in CBPV
                // closes over thunked sibling references, not raw computations.
                //
                // `group.rs` constrains LetRec RHS to lambda/block, both of
                // which elaborate to `Return(Thunk(arc))`; project the thunk
                // out, no hoisting possible.
                let scope = self.current_scope_mut();
                for (name, _) in &bindings {
                    scope.insert(name.clone());
                }
                let elab: Vec<(String, Val)> = bindings
                    .iter()
                    .map(|(name, value)| {
                        let mut empty = Vec::new();
                        let CompKind::Return(Val::Thunk(arc)) =
                            self.elab_expr(value, &mut empty).item
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
                        (name.clone(), Val::Thunk(arc))
                    })
                    .collect();
                comp!(
                    self,
                    CompKind::LetRec {
                        slot: None,
                        bindings: Arc::new(elab),
                    }
                )
            }
        }
    }

    /// Elaborate one statement.  This is the binding boundary: any hoisted
    /// sub-expression binds accumulated during elaboration of the statement
    /// are wrapped here with `wrap_binds`.
    fn stmt(&mut self, ast: &Ast) -> Comp {
        match ast {
            Ast::Let { pattern, value } => {
                // Elaborate the value under its own span so pattern-bind
                // unify failures underline `42` in `let [a, b] = 42`,
                // not the whole let statement.  The outer Bind Comp
                // still carries the stmt span (restored on closure return).
                let mut binds = Vec::new();
                let comp =
                    self.with_span(value.span, |this| this.elab_expr(&value.item, &mut binds));
                // Pattern elaboration runs under the pattern's own span so
                // any pattern-shape diagnostic from default elaboration
                // (or future pattern-time checks) narrows onto the pattern.
                let pattern_ir =
                    self.with_span(pattern.span, |this| this.elab_pattern(&pattern.item));
                let inner = comp!(
                    self,
                    CompKind::Bind {
                        comp: Arc::new(comp),
                        pattern: pattern_ir,
                        rest: Arc::new(comp!(self, CompKind::Return(Val::Unit))),
                        scheme: None,
                        rhs_output: crate::mode::ByteMode::Empty,
                    }
                );
                wrap_binds(self.current_span, binds, inner)
            }

            other => {
                let mut binds = Vec::new();
                let comp = self.elab_expr(other, &mut binds);
                wrap_binds(self.current_span, binds, comp)
            }
        }
    }

    /// Elaborate `ast` as a computation.
    ///
    /// Any sub-expression that must be evaluated before its parent — because
    /// the IR only allows `Val` in argument position — is bound to a fresh
    /// temporary and pushed into `binds`.  The caller is responsible for
    /// calling `wrap_binds(binds, comp)` at the appropriate statement
    /// boundary to produce the final `Comp::Bind` chain.
    fn elab_expr(&mut self, ast: &Ast, binds: &mut Vec<(IrPattern, Comp)>) -> Comp {
        match ast {
            Ast::Word(Word::Plain(s)) | Ast::Word(Word::Slash(s)) => {
                comp!(self, CompKind::Return(Val::from_word(s)))
            }
            Ast::Literal(s) => comp!(self, CompKind::Return(Val::String(s.clone()))),
            Ast::Variable(s) => comp!(self, CompKind::Return(Val::Variable(s.clone()))),
            Ast::Word(Word::Tilde(path)) => {
                comp!(self, CompKind::Return(Val::TildePath(path.clone())))
            }

            Ast::Block(body) => {
                let body_comp = self.with_new_scope(|this| this.stmts(body));
                comp!(self, CompKind::Return(Val::Thunk(Arc::new(body_comp))))
            }

            Ast::Lambda { param, body } => {
                // Elaborate defaults *before* the param's own names enter
                // scope — defaults reference the outer environment.  The
                // param's own span narrows any pattern-shape diagnostic
                // onto the parameter rather than the whole lambda.
                let param_ir = self.with_span(param.span, |this| this.elab_pattern(&param.item));
                let mut names = HashSet::new();
                param.item.collect_names(&mut names);
                let body_comp = self.with_bound_names(names, |this| this.stmts(body));
                // Flatten `return { |p| M }` → `return thunk(lam p. M)` when body
                // is already a single lambda (avoids double-wrapping).
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
                head: Head::Bare(s),
                args,
                redirects,
            } if s == "echo" && !self.is_bound("echo") => self.expand_echo(args, redirects, binds),

            Ast::Call {
                head,
                args,
                redirects,
            } => {
                // Lower args into the IR's positional-arg shape.
                // Argument-position spread (`Ast::Spread`) becomes
                // `ValListElem::Spread`; ordinary args become `Single`.
                // A list-literal argument `f [...$xs]` stays one arg
                // whose own list elements carry the spread.  Each arg's
                // `Spanned` from the AST is preserved on the IR element
                // so per-arg diagnostics narrow uniformly downstream.
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

                // Classify the head into CBPV App (bound/value heads) or
                // shell Exec (name-dispatched heads).
                match head {
                    Head::ExternalName(s) => {
                        self.exec(CommandName::Bare(s.clone()), arg_vals, redirect_vals, true)
                    }
                    Head::Bare(s) if self.is_bound(s) => {
                        // The Force is what makes `step_force` run a block
                        // under any wrapping redirect.
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
                    Head::Value(value) => {
                        // Warn on `{ … } < file` and friends: a literal block
                        // is `Return(Thunk(…))` — a value, not a command.  The
                        // block does still execute (eval_app trampolines a
                        // Thunk in head position so users with bound
                        // wrappers like `let f = { … }; f < file` keep
                        // working), but the redirect lands on a value-form
                        // and is almost always inert.  If the author meant
                        // "run this block under the redirect", the right
                        // forms are `let f = { … }; f < file` (bind first)
                        // or `!{ … } < file` (force).
                        if matches!(value.as_ref(), Ast::Block(_)) && !redirect_vals.is_empty() {
                            crate::diagnostic::shell_warning(
                                "redirect on a `{ … }` literal: the block is a \
                                 value, not a command — the redirect has no \
                                 consumer.  Bind first (`let f = { … }; f < file`) \
                                 or force (`!{ … } < file`).",
                            );
                        }
                        let head_comp = self.elab_expr(value, binds);
                        self.apply_head(head_comp, arg_vals, redirect_vals)
                    }
                }
            }

            Ast::Scope { op, redirects } => {
                let redirect_vals = self.lower_redirects(redirects, binds);
                let scope_op = match op {
                    ScopeAst::Try { body, handler } => ScopeOp::Try {
                        body: self.to_val(body, binds),
                        handler: self.to_val(handler, binds),
                    },
                    ScopeAst::Guard { body, cleanup } => ScopeOp::Guard {
                        body: self.to_val(body, binds),
                        cleanup: self.to_val(cleanup, binds),
                    },
                    ScopeAst::Within { opts, body } => ScopeOp::Within {
                        opts: self.to_val(opts, binds),
                        body: self.to_val(body, binds),
                    },
                    ScopeAst::Grant { caps, body } => ScopeOp::Grant {
                        caps: self.to_val(caps, binds),
                        body: self.to_val(body, binds),
                    },
                    ScopeAst::Audit { body } => ScopeOp::Audit {
                        body: self.to_val(body, binds),
                    },
                };
                let inner = comp!(self, CompKind::Scope(scope_op));
                self.wrap_redirect(inner, redirect_vals)
            }

            Ast::Return(None) => comp!(self, CompKind::Return(Val::Unit)),

            Ast::Return(Some(value)) => self.with_span(value.span, |this| {
                // Narrow `current_span` to the value expression's own
                // range so the Return Comp inherits it — any error
                // fired while inferring the value (heterogeneous
                // list, wrong variant payload, …) underlines that
                // expression rather than the `return …` statement.
                comp!(this, CompKind::Return(this.to_val(&value.item, binds)))
            }),

            Ast::Pipeline(stages) => {
                let mut comps = Vec::new();
                for stage in stages {
                    // Stamp the stage's span before lowering it, so
                    // diagnostics inside the stage point at the stage's
                    // first token rather than the enclosing pipeline's.
                    // `with_span` restores on closure return so the next
                    // iteration starts from a clean span.  A stage's hoists
                    // belong inside the stage; a surface `{ … }` stage is a
                    // thunk the pipeline drives (a producer is forced, a
                    // consumer has the upstream value applied to it), not
                    // inline statements — so isolate binds without the
                    // inline-block reading `elab_guarded` gives an `if` arm.
                    let stage_comp =
                        self.with_span(stage.span, |this| this.elab_isolated(&stage.item));
                    comps.push(Arc::new(stage_comp));
                }
                // The elaborator emits the all-`Empty` placeholder wire the
                // annotation pass overwrites — one per stage, matching the
                // `Var → Empty` default the checker grounds an unconstrained
                // mode to.  The checker runs on every evaluated comp, so an
                // un-annotated pipeline never reaches the evaluator.
                let wires = vec![crate::mode::Wire::EMPTY; comps.len()];
                // A `Unit` placeholder per stage, overwritten by the
                // annotation pass with each stage's resolved value type.
                let stage_types = vec![crate::typecheck::Ty::Unit; comps.len()];
                comp!(
                    self,
                    CompKind::Pipeline {
                        stages: comps,
                        wires,
                        stage_types
                    }
                )
            }

            Ast::Chain(parts) => {
                // Every arm but the first runs only when its predecessors
                // failed, so each arm is a guarded context: its hoists
                // must stay inside the arm, never run unconditionally in
                // the caller.  Routing through `elab_guarded` gives each
                // arm its own binds vector.
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

            Ast::Background(value) => self.with_span(value.span, |this| {
                // `cmd &` is pure sugar for `spawn { cmd }`: the inner
                // pipeline becomes a thunk (its hoists stay inside its own
                // body) handed to the name-dispatched `spawn` builtin.  Lower
                // it that way rather than to a dedicated IR node, so the
                // launch goes through the audited command path and needs no
                // parallel typecheck/eval/walk arms.
                let body = this.elab_branch(&value.item);
                let arg = Spanned::with_span(value.span, ValListElem::Single(Val::Thunk(body)));
                this.exec(
                    CommandName::Bare("spawn".into()),
                    vec![arg],
                    Vec::new(),
                    false,
                )
            }),

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
                                Val::Variable(name.clone()),
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

            Ast::If { branches, else_ } => self.elab_if(branches, else_, binds),

            Ast::Case { scrutinee, table } => comp!(
                self,
                CompKind::Case {
                    scrutinee: Spanned::with_span(
                        scrutinee.span,
                        self.with_span(scrutinee.span, |this| {
                            this.to_val(&scrutinee.item, binds)
                        }),
                    ),
                    table: Spanned::with_span(
                        table.span,
                        self.with_span(table.span, |this| { this.to_val(&table.item, binds) }),
                    ),
                }
            ),

            Ast::Let { .. } => unreachable!("assignment in elab_expr"),

            // `Ast::Spread` is parser-emitted only as an immediate child
            // of `Ast::Call`'s `args`; the Call arm consumes it directly
            // (above) and never delegates to `elab_expr`.  Anywhere else
            // is a parser invariant violation.
            Ast::Spread(_) => {
                unreachable!("Ast::Spread must be consumed by Ast::Call's arg lowering")
            }
        }
    }

    /// Elaborate `if cond body [elsif cond body]* [else body]` into nested
    /// `CompKind::If` nodes.
    ///
    /// One-armed form (single branch, no else) wraps the body as
    /// `{ !body; unit }` so both sides return `Unit` — the branch is evaluated
    /// for side effects only.  Multi-armed forms require every branch and the
    /// `else` to agree on their return type; the typechecker enforces this.
    ///
    /// The first branch's cond hoists into the caller's `binds` because it
    /// is always evaluated; every later branch's cond hoists into a local
    /// binds vector wrapped around the nested else-arm, so the cond's
    /// effects only fire when prior branches missed.
    fn elab_if(
        &mut self,
        branches: &[IfBranch],
        else_: &Option<Spanned<Box<Ast>>>,
        binds: &mut Vec<(IrPattern, Comp)>,
    ) -> Comp {
        let (first, rest) = branches
            .split_first()
            .expect("if must have at least one branch");
        let one_armed = rest.is_empty() && else_.is_none();

        // Walk the elsif tail from the back, folding each into the
        // running else-arm.
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

        // Primary branch: hoists are unconditional, so they ride the caller's binds.
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

    /// Elaborate `ast` as a branch computation.  Hoists from `ast` stay
    /// branch-local — they run only when the branch is chosen.
    ///
    /// A surface `{ stmts }` block means "run these stmts inline", so we
    /// elaborate the statement list directly.  A lambda or any other
    /// expression elaborates to whatever Comp naturally yields its value
    /// (typically `Return(Thunk(Lam))` for a lambda, an `App` for a
    /// command call, etc.); the If evaluator just runs it.  Tail position
    /// propagates through `with_scope` around the branch unchanged, so
    /// tail calls inside an inline branch still TCO.
    fn elab_branch(&mut self, ast: &Ast) -> Arc<Comp> {
        Arc::new(self.elab_guarded(ast))
    }

    /// Elaborate `ast` in a conditional context — a context that may or
    /// may not run it.  The defining property is the *fresh* binds vector:
    /// any effectful sub-expression `ast` hoists is wrapped right here,
    /// inside the returned `Comp`, never threaded into a caller's
    /// accumulator.  A conditional context that routes through this entry
    /// therefore physically cannot hoist an untaken arm's effects past
    /// the guard.
    ///
    /// A surface `{ stmts }` block means "run these stmts inline", so it
    /// elaborates the statement list directly; any other expression
    /// elaborates to whatever `Comp` naturally yields its value.  Tail
    /// position propagates through the inline branch unchanged.
    fn elab_guarded(&mut self, ast: &Ast) -> Comp {
        let mut branch_binds = Vec::new();
        let body = match ast {
            Ast::Block(stmts) => self.with_new_scope(|this| this.stmts(stmts)),
            _ => self.elab_expr(ast, &mut branch_binds),
        };
        wrap_binds(self.current_span, branch_binds, body)
    }

    /// Elaborate `ast` as a pipeline stage: isolate its hoisted binds in a
    /// fresh accumulator wrapped into the returned `Comp`, but — unlike
    /// [`Self::elab_guarded`] — read a surface `{ … }` block as the *thunk
    /// value* it denotes rather than as inline statements.  A pipeline
    /// stage is data the pipeline drives: a producer block is forced for
    /// its value at the value edge, and a consumer block has the upstream
    /// value applied to it.  Reading the block inline would instead splice
    /// the upstream value onto the block's final command.
    fn elab_isolated(&mut self, ast: &Ast) -> Comp {
        let mut stage_binds = Vec::new();
        let body = self.elab_expr(ast, &mut stage_binds);
        wrap_binds(self.current_span, stage_binds, body)
    }

    /// Elaborate `ast` as a branch and discard its result.  Used by
    /// one-armed `if` so the chosen-branch type is `F Unit`.
    fn elab_branch_unit(&mut self, ast: &Ast) -> Arc<Comp> {
        let body = self.elab_branch(ast);
        // Use Seq rather than Bind so the branch's stdout flows through to
        // the parent — Bind would capture it via eval_bind_rhs.
        Arc::new(comp!(
            self,
            CompKind::Seq(vec![
                body,
                Arc::new(comp!(self, CompKind::Return(Val::Unit))),
            ])
        ))
    }

    /// Hoist a `Comp` into `binds` and yield the `Val` the parent consumes.
    /// `Return(v)` passes through; anything else is bound to a fresh `_gN`.
    fn hoist(&mut self, comp: Comp, binds: &mut Vec<(IrPattern, Comp)>) -> Val {
        match comp.item {
            CompKind::Return(v) => v,
            _ => {
                let name = self.gensym();
                binds.push((IrPattern::Name(name.clone()), comp));
                Val::Variable(name)
            }
        }
    }

    /// Convert `ast` to a `Val`, hoisting any effectful computation into `binds`.
    #[allow(clippy::wrong_self_convention)]
    fn to_val(&mut self, ast: &Ast, binds: &mut Vec<(IrPattern, Comp)>) -> Val {
        let comp = self.elab_expr(ast, binds);
        self.hoist(comp, binds)
    }

    /// Apply a callable `head_comp` to `arg_vals` and wrap any trailing
    /// `redirects`.  Shared by the two value-application heads — a bound
    /// bare name (`f x`) and an explicit value head (`$f x`, `{…} x`) —
    /// which build the identical CBPV shape: a zero-arg "call" is just the
    /// head computation (`App { args: [] }` is not a CBPV form), and a
    /// non-empty call is an [`CompKind::App`]; either way trailing
    /// redirects ride a [`ScopeOp::Redirect`] frame via [`Self::wrap_redirect`].
    fn apply_head(&mut self, head_comp: Comp, arg_vals: Args, redirects: Vec<RedirectV>) -> Comp {
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

    /// Wrap `body` in a [`ScopeOp::Redirect`] frame if `redirects` is
    /// non-empty, otherwise return `body` unchanged.  Centralises the
    /// elaborator's "attach trailing redirects to a non-`Exec` body"
    /// pattern: `Exec` fuses its own redirects directly into the
    /// syscall, and pipelines/chains do not accept trailing redirects
    /// at the surface level; every other case (CBPV `App`, nested
    /// `Scope`) routes through here so the redirect lives as an
    /// effect-frame scope rather than a transparent wrapper.
    fn wrap_redirect(&mut self, body: Comp, redirects: Vec<RedirectV>) -> Comp {
        if redirects.is_empty() {
            return body;
        }
        comp!(
            self,
            CompKind::Scope(ScopeOp::Redirect {
                body: Arc::new(body),
                redirects,
            })
        )
    }

    /// Lower a parser-side [`Redirect`] list to the IR-side
    /// [`RedirectV`] shape, hoisting any effectful target expressions
    /// into `binds` exactly like [`to_val`] does for other values.
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

    /// Lower an `Ast::Expr` body to a single `Comp` that, when evaluated,
    /// produces the expression's value.  Intermediate computations are
    /// hoisted into `binds` exactly like `to_val` does for other effectful
    /// sub-expressions, so `$[a + b > 0]` unfolds to a flat sequence of
    /// `Comp::Bind` nodes at the enclosing statement boundary, with
    /// `PrimOp` leaves at the bottom.  There is no specialised IR for
    /// expressions — complex values decompose into CBPV primitives.
    fn lower_expr(&mut self, expr: &Expr, binds: &mut Vec<(IrPattern, Comp)>) -> Comp {
        // `And` / `Or` are the only short-circuiting forms; everything
        // else evaluates all operands strictly.
        match expr {
            Expr::Integer(n) => comp!(self, CompKind::Return(Val::Int(*n))),
            Expr::Number(n) => comp!(self, CompKind::Return(Val::Float(*n))),
            Expr::Bool(b) => comp!(self, CompKind::Return(Val::Bool(*b))),
            Expr::Variable(name) => comp!(self, CompKind::Return(Val::Variable(name.clone()))),
            Expr::Index(name, keys) => comp!(
                self,
                CompKind::Index {
                    target: Val::Variable(name.clone()),
                    keys: keys
                        .iter()
                        .map(|k| Spanned::with_span(
                            k.span,
                            self.with_span(k.span, |this| this.to_val(&k.item, binds)),
                        ))
                        .collect(),
                }
            ),
            Expr::Force(inner) => self.with_span(inner.span, |this| {
                comp!(this, CompKind::Force(this.to_val(&inner.item, binds)))
            }),
            Expr::BinOp(l, op, r) => {
                let lv = self.expr_to_val(l, binds);
                let rv = self.expr_to_val(r, binds);
                comp!(self, CompKind::Binary(*op, lv, rv))
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

    /// Desugar `a && b` / `a || b` to `_if a { … } { … }`.  The RHS lowers
    /// in an isolated `binds` vector so its effectful sub-expressions stay
    /// inside the short-circuited branch.
    fn lower_short_circuit(
        &mut self,
        l: &Expr,
        r: &Expr,
        binds: &mut Vec<(IrPattern, Comp)>,
        on_true_is_rhs: bool,
    ) -> Comp {
        let cond = self.expr_to_val(l, binds);
        // RHS is evaluated only conditionally, so its binds must not
        // escape into the enclosing scope.
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
                // Short-circuit `&&`/`||` is synthesised — no surface
                // cond to point at — so `None` falls back to the
                // enclosing pos via `with_span`'s no-op branch.
                cond: Spanned::synthetic(cond),
                then: Arc::new(then_branch),
                else_: Arc::new(else_branch),
            }
        )
    }
    /// Lower `echo <args...>` to `to-line !{ intercalate " " [str a, str b, ...] }`.
    ///
    /// Each argument is rendered via `str` before being collected into a list,
    /// so unlike arguments share no type — the render-then-collect strategy
    /// that makes echo work without a top type.  Spreads become `map str $xs`.
    /// This is surface sugar per ADR 260622.
    fn expand_echo(
        &mut self,
        args: &[Spanned<Ast>],
        redirects: &[Redirect],
        binds: &mut Vec<(IrPattern, Comp)>,
    ) -> Comp {
        use crate::syntax::ast::{Ast, Head, ListElem};

        let mut list_elems: Vec<ListElem> = Vec::new();

        for arg in args {
            let elem = match &arg.item {
                Ast::Spread(inner) => {
                    let map_call = Ast::Call {
                        head: Head::Bare("map".into()),
                        args: vec![
                            Spanned::synthetic(Ast::Variable("str".into())),
                            Spanned::synthetic((*inner.item).clone()),
                        ],
                        redirects: vec![],
                    };
                    ListElem::Spread(Spanned::synthetic(map_call))
                }
                other => {
                    let str_call = Ast::Call {
                        head: Head::Bare("str".into()),
                        args: vec![Spanned::synthetic(other.clone())],
                        redirects: vec![],
                    };
                    ListElem::Single(Spanned::synthetic(str_call))
                }
            };
            list_elems.push(elem);
        }

        let intercalate_call = Ast::Call {
            head: Head::Bare("intercalate".into()),
            args: vec![
                Spanned::synthetic(Ast::Literal(" ".into())),
                Spanned::synthetic(Ast::List(list_elems)),
            ],
            redirects: vec![],
        };

        let thunk_body = vec![Spanned::synthetic(intercalate_call)];
        let force = Ast::Force(Spanned::synthetic(Box::new(Ast::Block(thunk_body))));

        let lowered = Ast::Call {
            head: Head::Bare("to-line".into()),
            args: vec![Spanned::synthetic(force)],
            redirects: redirects.to_vec(),
        };
        self.elab_expr(&lowered, binds)
    }
}

/// Fold an accumulated list of `(pattern, comp)` bindings around an inner
/// computation, producing a chain of `Comp::Bind` nodes.  Reused by
/// short-circuit lowering to keep conditional-branch binds local.
fn wrap_binds(span: Option<Span>, binds: Vec<(IrPattern, Comp)>, inner: Comp) -> Comp {
    binds
        .into_iter()
        .rev()
        .fold(inner, |rest, (pattern, comp)| {
            Spanned::with_span(
                span,
                CompKind::Bind {
                    comp: Arc::new(comp),
                    pattern,
                    rest: Arc::new(rest),
                    scheme: None,
                    rhs_output: crate::mode::ByteMode::Empty,
                },
            )
        })
}

/// Sugar `exit` / `quit` with no arguments into `exit 0` / `quit 0`.
///
/// The runtime builtin accepts 0 or 1 arg (defaulting to status 0), but
/// ral's "fixed arity always" invariant means the *scheme* is `Int → F Unit`.
/// Rather than special-case zero-arg in the typechecker, we desugar here so
/// the call always carries a status argument by the time the IR lands.
fn desugar_zero_arg_exit(name: &str, args: Args) -> Args {
    if args.is_empty() && (name == "exit" || name == "quit") {
        vec![Spanned::synthetic(ValListElem::Single(Val::Int(0)))]
    } else {
        args
    }
}

/// Return the set of names exported by the prelude (cached after first call).
fn prelude_scope() -> HashSet<String> {
    static PRELUDE: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();
    PRELUDE
        .get_or_init(|| {
            prelude_manifest::PRELUDE_EXPORTS
                .iter()
                .map(|s| s.to_string())
                .collect()
        })
        .clone()
}

/// Elaborate a top-level statement sequence into a single [`Comp`].
///
/// `bindings` is the set of names already bound in the calling
/// environment (e.g. accumulated REPL definitions).  The prelude exports
/// are always in scope.
///
/// If the `RAL_DUMP_IR` environment variable is set, the resulting IR is
/// printed to stderr before being returned.
pub fn elaborate(ast: &[Stmt], bindings: HashSet<String>) -> Comp {
    let comp = Elaborator::new_with_bindings(bindings).stmts(ast);
    if std::env::var("RAL_DUMP_IR").is_ok() {
        eprintln!("{comp:#?}");
    }
    comp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::parser::parse;

    /// Pattern-match a `Comp` as an `Exec` and pull out the bits the
    /// tests care about (the head name, the args, and whether the
    /// head was written as `^name`).  Panics on shape mismatch.
    fn expect_exec_name(comp: &Comp) -> (&CommandName, &Args, bool) {
        let CompKind::Exec(e) = &comp.item else {
            panic!("expected exec, got {:?}", comp.item);
        };
        let external_only = matches!(e.head, CommandWord::External(_));
        (e.head.name(), &e.args, external_only)
    }

    /// Strip the `Spanned` wrapper off each arg so test assertions can
    /// match on the structural shape without caring about source spans
    /// (which depend on parser positions).
    fn arg_items(args: &Args) -> Vec<ValListElem> {
        args.iter().map(|s| s.item.clone()).collect()
    }

    #[test]
    fn tilde_path_command_head_elaborates_to_exec() {
        let ast = parse("~/.local/bin/claude update").expect("parse");
        let comp = elaborate(&ast, HashSet::new());
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
        let comp = elaborate(&ast, HashSet::new());
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
        let comp = elaborate(&ast, HashSet::new());
        let (name, args, _) = expect_exec_name(&comp);
        assert_eq!(name, &CommandName::Path("./script".into()));
        assert!(args.is_empty());
    }

    #[test]
    fn external_name_head_elaborates_to_external_exec() {
        let ast = parse("^git status").expect("parse");
        let comp = elaborate(&ast, HashSet::new());
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
        // Head::Value (`$map`) elaborates to App with the inner Comp directly,
        // *without* a wrapping Force.  The autoforce happens at runtime when
        // eval_app sees a Thunk in head position.  This keeps `<file`
        // redirects on the App able to bracket the body — see the
        // `with_redirects → install_stdin_redirect` path.
        let ast = parse("$map $upper ['a']").expect("parse");
        let comp = elaborate(&ast, HashSet::new());
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

    /// F8: a command use of a name precedes a `let` that rebinds the
    /// same name to a *non-thunk* RHS.  Only thunk-form bindings are
    /// forward-declared (the shapes `group.rs` knots), so the earlier
    /// `date` must still resolve as an external command — an `Exec`,
    /// not a `Force(Variable("date"))` that dies as an undefined
    /// variable at runtime.
    #[test]
    fn command_use_before_non_thunk_let_is_exec() {
        let ast = parse("date\nlet date = 5").expect("parse");
        let comp = elaborate(&ast, HashSet::new());
        let CompKind::Seq(parts) = &comp.item else {
            panic!("expected a Seq of two statements, got {:?}", comp.item);
        };
        let (name, _, _) = expect_exec_name(&parts[0]);
        assert_eq!(name, &CommandName::Bare("date".into()));
    }

    /// F8: forward-declaration is scoped to the recursive group's own RHS
    /// bodies, not to preceding statements.  An acyclic singleton
    /// `let f = { return 1 }` emits as a `Single` (`Comp::Bind`), so an
    /// earlier use of `f` sees no binding and resolves to an external
    /// command `Exec` — the binding's `Bind` runs after the use, so a
    /// `Force(Variable("f"))` there would die as an undefined variable.
    #[test]
    fn command_use_before_acyclic_thunk_let_is_exec() {
        let ast = parse("f\nlet f = { return 1 }").expect("parse");
        let comp = elaborate(&ast, HashSet::new());
        let CompKind::Seq(parts) = &comp.item else {
            panic!("expected a Seq, got {:?}", comp.item);
        };
        let (name, _, _) = expect_exec_name(&parts[0]);
        assert_eq!(name, &CommandName::Bare("f".into()));
    }

    /// F8: the same holds for a self-recursive group — a use of `g` before
    /// its definition is a `Single` emitted ahead of the `LetRec`, so it
    /// lowers to `Exec`, while the self-reference inside the group's own
    /// body resolves to the forward-declared binding.
    #[test]
    fn command_use_before_recursive_thunk_let_is_exec() {
        let ast = parse("g 3\nlet g = { |n| g $[$n - 1] }").expect("parse");
        let comp = elaborate(&ast, HashSet::new());
        let CompKind::Seq(parts) = &comp.item else {
            panic!("expected a Seq, got {:?}", comp.item);
        };
        let (name, _, _) = expect_exec_name(&parts[0]);
        assert_eq!(name, &CommandName::Bare("g".into()));
        assert!(
            matches!(parts[1].item, CompKind::LetRec { .. }),
            "expected the self-recursive binding to emit a LetRec, got {:?}",
            parts[1].item
        );
    }

    /// F8: intra-group recursion still resolves to the binding.  In
    /// `let f = { |n| f $n }\nf 5`, the self-reference inside `f`'s body
    /// forces the forward-declared variable rather than shelling out.
    #[test]
    fn intra_group_recursion_resolves_to_binding() {
        let ast = parse("let f = { |n| f $n }\nf 5").expect("parse");
        let comp = elaborate(&ast, HashSet::new());
        let CompKind::Seq(parts) = &comp.item else {
            panic!("expected a Seq, got {:?}", comp.item);
        };
        let CompKind::LetRec { bindings, .. } = &parts[0].item else {
            panic!("expected a LetRec group, got {:?}", parts[0].item);
        };
        let Val::Thunk(body) = &bindings[0].1 else {
            panic!("expected a thunked lambda binding, got {:?}", bindings[0].1);
        };
        let CompKind::Lam { body, .. } = &body.item else {
            panic!("expected a lambda RHS, got {:?}", body.item);
        };
        assert!(
            matches!(body.item, CompKind::App { .. }),
            "expected the self-reference to force the bound variable, got {:?}",
            body.item
        );
    }

    /// F3: a `?`-chain arm is a guarded context — its hoisted effects
    /// must stay inside the arm, never be lifted into the caller's bind
    /// chain (where they would run unconditionally before the chain).
    /// The interpolation's `!{…}` force hoists; after the fix the
    /// statement elaborates to a bare `Chain`, with the hoist living
    /// inside the second arm rather than as an enclosing `Bind`.
    #[test]
    fn chain_arm_hoist_stays_inside_the_arm() {
        let ast = parse(r#"return ok ? echo "fallback: !{hostname}""#).expect("parse");
        let comp = elaborate(&ast, HashSet::new());
        assert!(
            matches!(comp.item, CompKind::Chain(_)),
            "chain arm hoist leaked into the caller: expected a bare Chain, got {:?}",
            comp.item
        );
    }
}
