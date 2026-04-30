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
//! *binds* accumulator (`Vec<(Pattern, Comp)>`) through `elab_expr`; callers
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
//! - An external-name head (`^name`) → `Comp::Exec { name, external_only: true, … }`
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

use crate::ast::*;
use crate::group::{StmtGroup, group_stmts};
use crate::ir::*;
use crate::prelude_manifest;
use crate::span::Span;
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
    current_span: Option<Span>,
}

/// Wrap a `CompKind` using the elaborator's current span.
macro_rules! comp {
    ($self:expr, $kind:expr) => {
        Comp::with_span($self.current_span, $kind)
    };
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

    /// Record all names introduced by `pat` in the current scope.
    fn bind_pattern(&mut self, pat: &Pattern) {
        pat.collect_names(self.lexical_scopes.last_mut().unwrap());
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
        name: ExecName,
        args: Vec<Val>,
        redirects: Vec<(u32, RedirectMode, ValRedirectTarget)>,
        external_only: bool,
    ) -> Comp {
        comp!(
            self,
            CompKind::Exec {
                name,
                args,
                redirects,
                external_only,
            }
        )
    }

    /// Elaborate a statement sequence into a single `Comp`.
    /// An empty sequence returns `Comp::Return(Val::Unit)`.
    /// A single-element sequence returns that element's `Comp` unwrapped.
    /// Multiple elements are wrapped in `Comp::Seq`.
    fn stmts(&mut self, stmts: &[Ast]) -> Comp {
        // Forward-declare all named let-bindings so that bare-name resolution
        // (which happens during elaboration) sees them even before their
        // definition point.  This is what allows mutual recursion to work
        // across arbitrary non-let statements.
        if let Some(scope) = self.lexical_scopes.last_mut() {
            for stmt in stmts {
                if let Ast::Let {
                    pattern: Pattern::Name(name),
                    ..
                } = stmt
                {
                    scope.insert(name.clone());
                }
            }
        }
        let groups = group_stmts(stmts);
        let comps: Vec<Comp> = groups
            .into_iter()
            .filter_map(|g| self.emit_group(g))
            .collect();
        match comps.len() {
            0 => comp!(self, CompKind::Return(Val::Unit)),
            1 => comps.into_iter().next().unwrap(),
            _ => comp!(self, CompKind::Seq(comps)),
        }
    }

    /// Elaborate a single statement group (produced by the [`group`] pre-pass).
    ///
    /// `Pos` nodes update the current span and emit nothing.  Single
    /// statements elaborate normally.  `LetRec` groups elaborate each
    /// binding body (unwrapping `Return(Thunk(…))` to expose the inner
    /// lambda) and emit a `CompKind::LetRec` node.
    fn emit_group(&mut self, group: StmtGroup) -> Option<Comp> {
        match group {
            StmtGroup::Single(Ast::Pos(s)) => {
                self.current_span = Some(s);
                None
            }
            StmtGroup::Single(ast) => {
                let comp = self.stmt(&ast);
                if let Ast::Let { pattern, .. } = &ast {
                    self.bind_pattern(pattern);
                }
                Some(comp)
            }
            StmtGroup::LetRec(bindings) => {
                // All binding names are already in scope (forward-declared by
                // `stmts`), so no extra scoping is needed here.
                let elab: Vec<(String, Comp)> = bindings
                    .iter()
                    .map(|(name, value)| {
                        let mut binds = Vec::new();
                        let comp = self.elab_expr(value, &mut binds);
                        let lam = match &comp.kind {
                            CompKind::Return(Val::Thunk(arc)) => arc.as_ref().clone(),
                            _ => comp,
                        };
                        (name.clone(), lam)
                    })
                    .collect();
                Some(comp!(
                    self,
                    CompKind::LetRec {
                        slot: None,
                        bindings: Arc::new(elab),
                    }
                ))
            }
        }
    }

    /// Elaborate one statement.  This is the binding boundary: any hoisted
    /// sub-expression binds accumulated during elaboration of the statement
    /// are wrapped here with `wrap_binds`.
    fn stmt(&mut self, ast: &Ast) -> Comp {
        match ast {
            Ast::Pos(s) => {
                // Pos at statement level is handled by emit_group; this arm
                // covers the rare case where Pos appears inside an expression.
                self.current_span = Some(*s);
                comp!(self, CompKind::Return(Val::Unit))
            }

            Ast::Let { pattern, value } => {
                let mut binds = Vec::new();
                let comp = self.elab_expr(value, &mut binds);
                let inner = comp!(
                    self,
                    CompKind::Bind {
                        comp: Box::new(comp),
                        pattern: pattern.clone(),
                        rest: Box::new(comp!(self, CompKind::Return(Val::Unit))),
                    }
                );
                wrap_binds(&self.current_span, binds, inner)
            }

            other => {
                let mut binds = Vec::new();
                let comp = self.elab_expr(other, &mut binds);
                wrap_binds(&self.current_span, binds, comp)
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
    fn elab_expr(&mut self, ast: &Ast, binds: &mut Vec<(Pattern, Comp)>) -> Comp {
        match ast {
            Ast::Word(Word::Plain(s)) | Ast::Word(Word::Slash(s)) => {
                comp!(self, CompKind::Return(Val::Literal(s.clone())))
            }
            Ast::Literal(s) => comp!(self, CompKind::Return(Val::String(s.clone()))),
            Ast::Variable(s) => comp!(self, CompKind::Return(Val::Variable(s.clone()))),
            Ast::Word(Word::Tilde(path)) => {
                comp!(self, CompKind::Return(Val::TildePath(path.clone())))
            }
            Ast::Pos(s) => {
                self.current_span = Some(*s);
                comp!(self, CompKind::Return(Val::Unit))
            }

            Ast::Block(body) => {
                let body_comp =
                    self.with_bound_names(std::iter::empty::<String>(), |this| this.stmts(body));
                comp!(self, CompKind::Return(Val::Thunk(Arc::new(body_comp))))
            }

            Ast::Lambda { param, body } => {
                let mut names = HashSet::new();
                param.collect_names(&mut names);
                let body_comp = self.with_bound_names(names, |this| this.stmts(body));
                // Flatten `return { |p| M }` → `return thunk(lam p. M)` when body
                // is already a single lambda (avoids double-wrapping).
                let body_comp = match &body_comp.kind {
                    CompKind::Return(Val::Thunk(inner))
                        if matches!(inner.as_ref().kind, CompKind::Lam { .. }) =>
                    {
                        inner.as_ref().clone()
                    }
                    _ => body_comp,
                };
                comp!(
                    self,
                    CompKind::Return(Val::Thunk(Arc::new(comp!(
                        self,
                        CompKind::Lam {
                            param: param.clone(),
                            body: Box::new(body_comp),
                        }
                    ))))
                )
            }

            Ast::Force(inner) => {
                let comp = self.elab_expr(inner, binds);
                force_from_comp(self.current_span, comp)
            }

            Ast::App { head, args, span } => {
                // The App's span covers head + all args; stamp it as the
                // current span so the resulting `Comp` (and any unification
                // failures it provokes) underline the whole command.
                self.current_span = Some(*span);
                // Partition args into plain values and I/O redirects.
                let mut arg_vals = Vec::new();
                let mut redirects = Vec::new();
                for arg in args {
                    match arg {
                        Ast::Redirect { fd, mode, target } => {
                            let t = match target {
                                RedirectTarget::File(a) => {
                                    ValRedirectTarget::File(self.to_val(a, binds))
                                }
                                RedirectTarget::Fd(n) => ValRedirectTarget::Fd(*n),
                            };
                            redirects.push((*fd, *mode, t));
                        }
                        _ => arg_vals.push(self.to_val(arg, binds)),
                    }
                }

                // Classify the head.
                match head {
                    // ^name: external-only dispatch — bypasses aliases/builtins/prelude
                    // but does NOT bypass within[handlers:] frames (containment).
                    Head::ExternalName(s) => {
                        self.exec(ExecName::Bare(s.clone()), arg_vals, redirects, true)
                    }
                    Head::Bare(s) if self.is_bound(s) => {
                        // Bound name: usually we Force the variable so that
                        // zero-arg closures evaluate on access.  But when there
                        // are redirects, the closure must run *under* them —
                        // hold it as a value here and let eval_app apply the
                        // redirects before forcing the body.
                        let head = if redirects.is_empty() {
                            comp!(self, CompKind::Force(Val::Variable(s.clone())))
                        } else {
                            comp!(self, CompKind::Return(Val::Variable(s.clone())))
                        };
                        comp!(
                            self,
                            CompKind::App {
                                head: Box::new(head),
                                args: arg_vals,
                                redirects,
                            }
                        )
                    }
                    Head::Bare(s) => {
                        self.exec(ExecName::Bare(s.clone()), arg_vals, redirects, false)
                    }
                    Head::Path(path) => {
                        self.exec(ExecName::Path(path.clone()), arg_vals, redirects, false)
                    }
                    Head::TildePath(path) => {
                        self.exec(ExecName::TildePath(path.clone()), arg_vals, redirects, false)
                    }
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
                        if matches!(value.as_ref(), Ast::Block(_)) && !redirects.is_empty() {
                            crate::diagnostic::shell_warning(
                                "redirect on a `{ … }` literal: the block is a \
                                 value, not a command — the redirect has no \
                                 consumer.  Bind first (`let f = { … }; f < file`) \
                                 or force (`!{ … } < file`).",
                            );
                        }
                        let head_comp = self.elab_expr(value, binds);
                        comp!(
                            self,
                            CompKind::App {
                                head: Box::new(head_comp),
                                args: arg_vals,
                                redirects,
                            }
                        )
                    }
                }
            }

            Ast::Return(None) => comp!(self, CompKind::Return(Val::Unit)),

            Ast::Return(Some(value)) => comp!(self, CompKind::Return(self.to_val(value, binds))),

            Ast::Pipeline(stages) => {
                let mut comps = Vec::new();
                for s in stages {
                    if let Ast::Pos(sp) = s {
                        self.current_span = Some(*sp);
                        continue;
                    }
                    let stage_comp = self.elab_expr(s, binds);
                    comps.push(stage_comp);
                }
                comp!(self, CompKind::Pipeline(comps))
            }

            Ast::Chain(parts) => {
                comp!(
                    self,
                    CompKind::Chain(parts.iter().map(|a| self.elab_expr(a, binds)).collect())
                )
            }

            Ast::Background(inner) => comp!(
                self,
                CompKind::Background(Box::new(self.elab_expr(inner, binds)))
            ),

            Ast::List(elems) => comp!(
                self,
                CompKind::Return(Val::List(
                    elems
                        .iter()
                        .map(|e| match e {
                            ListElem::Single(a) => ValListElem::Single(self.to_val(a, binds)),
                            ListElem::Spread(a) => ValListElem::Spread(self.to_val(a, binds)),
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
                            MapEntry::Entry(k, a) => {
                                ValMapEntry::Entry(self.to_val(k, binds), self.to_val(a, binds))
                            }
                            MapEntry::Spread(a) => ValMapEntry::Spread(self.to_val(a, binds)),
                        })
                        .collect(),
                ))
            ),

            Ast::Interpolation(parts) => {
                comp!(
                    self,
                    CompKind::Interpolation(parts.iter().map(|a| self.to_val(a, binds)).collect())
                )
            }

            Ast::Expr(expr) => self.lower_expr(expr, binds),

            Ast::Index { target, keys } => comp!(
                self,
                CompKind::Index {
                    target: Box::new(self.elab_expr(target, binds)),
                    keys: keys.iter().map(|k| self.elab_expr(k, binds)).collect(),
                }
            ),

            Ast::Redirect { fd, mode, target } => {
                let t = match target {
                    RedirectTarget::File(a) => ValRedirectTarget::File(self.to_val(a, binds)),
                    RedirectTarget::Fd(n) => ValRedirectTarget::Fd(*n),
                };
                comp!(
                    self,
                    CompKind::App {
                        head: Box::new(comp!(self, CompKind::Return(Val::String(String::new())))),
                        args: Vec::new(),
                        redirects: vec![(*fd, *mode, t)],
                    }
                )
            }

            Ast::If {
                cond,
                then,
                elsif,
                else_,
            } => self.elab_if(cond, then, elsif, else_, binds),

            Ast::Let { .. } => unreachable!("assignment in elab_expr"),
        }
    }

    /// Elaborate `if cond then [elsif cond then]* [else else_]` into nested
    /// `CompKind::If` nodes.
    ///
    /// One-armed form (no else, no elsif) wraps the then-branch as
    /// `{ !then; unit }` so both sides return `Unit` — the branch is evaluated
    /// for side effects only.  Multi-armed forms require both branches to agree
    /// on their return type; the typechecker enforces this.
    fn elab_if(
        &mut self,
        cond: &Ast,
        then: &Ast,
        elsif: &[(Ast, Ast)],
        else_: &Option<Box<Ast>>,
        binds: &mut Vec<(Pattern, Comp)>,
    ) -> Comp {
        let cond_comp = self.elab_expr(cond, binds);
        let one_armed = elsif.is_empty() && else_.is_none();
        let then_val = if one_armed {
            self.wrap_for_unit(then)
        } else {
            self.to_val(then, binds)
        };
        let else_val = self.build_else(elsif, else_, binds);
        comp!(
            self,
            CompKind::If {
                cond: Box::new(cond_comp),
                then: then_val,
                else_: else_val,
            }
        )
    }

    /// Elaborate `ast` as a thunk that forces it for side effects and returns
    /// `Unit`.  Used by one-armed `if` so the branch has type `U(F Unit)`
    /// regardless of the branch body's natural return type.
    fn wrap_for_unit(&mut self, ast: &Ast) -> Val {
        let inner = self.elab_expr(ast, &mut Vec::new());
        let forced = force_from_comp(self.current_span, inner);
        // Use Seq rather than Bind so the forced computation's stdout flows
        // through to the parent — Bind would capture it via eval_bind_rhs.
        let wrapped = comp!(
            self,
            CompKind::Seq(vec![forced, comp!(self, CompKind::Return(Val::Unit)),])
        );
        Val::Thunk(Arc::new(wrapped))
    }

    /// Build the else-side value for an `if` expression, recursively
    /// nesting `elsif` arms.  When there is no else branch at all, produces
    /// a `unit`-returning thunk so one-armed `if` has type `F Unit`.
    fn build_else(
        &mut self,
        elsif: &[(Ast, Ast)],
        else_: &Option<Box<Ast>>,
        binds: &mut Vec<(Pattern, Comp)>,
    ) -> Val {
        if let Some((ec, et)) = elsif.first() {
            // Nest remaining elsif/else into a thunk and make it the else branch.
            let inner_else = self.build_else(&elsif[1..], else_, binds);
            let nested_comp = comp!(
                self,
                CompKind::If {
                    cond: Box::new(self.elab_expr(ec, binds)),
                    then: self.to_val(et, binds),
                    else_: inner_else,
                }
            );
            Val::Thunk(Arc::new(nested_comp))
        } else if let Some(e) = else_ {
            self.to_val(e, binds)
        } else {
            Val::Thunk(Arc::new(comp!(self, CompKind::Return(Val::Unit))))
        }
    }

    /// Hoist a `Comp` into `binds` and yield the `Val` the parent consumes.
    /// `Return(v)` passes through; anything else is bound to a fresh `_gN`.
    fn hoist(&mut self, comp: Comp, binds: &mut Vec<(Pattern, Comp)>) -> Val {
        match comp.kind {
            CompKind::Return(v) => v,
            _ => {
                let name = self.gensym();
                binds.push((Pattern::Name(name.clone()), comp));
                Val::Variable(name)
            }
        }
    }

    /// Convert `ast` to a `Val`, hoisting any effectful computation into `binds`.
    #[allow(clippy::wrong_self_convention)]
    fn to_val(&mut self, ast: &Ast, binds: &mut Vec<(Pattern, Comp)>) -> Val {
        let comp = self.elab_expr(ast, binds);
        self.hoist(comp, binds)
    }

    /// Lower an `Ast::Expr` body to a single `Comp` that, when evaluated,
    /// produces the expression's value.  Intermediate computations are
    /// hoisted into `binds` exactly like `to_val` does for other effectful
    /// sub-expressions, so `$[a + b > 0]` unfolds to a flat sequence of
    /// `Comp::Bind` nodes at the enclosing statement boundary, with
    /// `PrimOp` leaves at the bottom.  There is no specialised IR for
    /// expressions — complex values decompose into CBPV primitives.
    fn lower_expr(&mut self, expr: &Expr, binds: &mut Vec<(Pattern, Comp)>) -> Comp {
        // `And` / `Or` are the only short-circuiting forms; everything
        // else evaluates all operands strictly.
        match expr {
            Expr::Integer(n) => comp!(self, CompKind::Return(Val::Int(*n))),
            Expr::Number(n) => comp!(self, CompKind::Return(Val::Float(*n))),
            Expr::Bool(b) => comp!(self, CompKind::Return(Val::Bool(*b))),
            Expr::Var(name) => comp!(self, CompKind::Return(Val::Variable(name.clone()))),
            Expr::Index(name, keys) => comp!(
                self,
                CompKind::Index {
                    target: Box::new(comp!(self, CompKind::Return(Val::Variable(name.clone())))),
                    keys: keys.iter().map(|k| self.elab_expr(k, binds)).collect(),
                }
            ),
            Expr::Force(inner) => force_from_comp(self.current_span, self.elab_expr(inner, binds)),
            Expr::BinOp(l, op, r) => {
                let lv = self.expr_to_val(l, binds);
                let rv = self.expr_to_val(r, binds);
                comp!(self, CompKind::PrimOp(*op, vec![lv, rv]))
            }
            Expr::Not(inner) => {
                let v = self.expr_to_val(inner, binds);
                comp!(self, CompKind::PrimOp(ExprOp::Not, vec![v]))
            }
            Expr::And(l, r) => self.lower_short_circuit(l, r, binds, /*on_true_is_rhs=*/ true),
            Expr::Or(l, r) => self.lower_short_circuit(l, r, binds, /*on_true_is_rhs=*/ false),
        }
    }

    /// `to_val` for `Expr` instead of `Ast`.
    fn expr_to_val(&mut self, expr: &Expr, binds: &mut Vec<(Pattern, Comp)>) -> Val {
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
        binds: &mut Vec<(Pattern, Comp)>,
        on_true_is_rhs: bool,
    ) -> Comp {
        let cond = self.expr_to_val(l, binds);
        // RHS is evaluated only conditionally, so its binds must not
        // escape into the enclosing scope.
        let mut r_binds = Vec::new();
        let r_comp = self.lower_expr(r, &mut r_binds);
        let r_comp = wrap_binds(&self.current_span, r_binds, r_comp);
        let short = comp!(self, CompKind::Return(Val::Bool(!on_true_is_rhs)));
        let (then_branch, else_branch) = if on_true_is_rhs {
            (r_comp, short)
        } else {
            (short, r_comp)
        };
        comp!(
            self,
            CompKind::If {
                cond: Box::new(comp!(self, CompKind::Return(cond))),
                then: Val::Thunk(Arc::new(then_branch)),
                else_: Val::Thunk(Arc::new(else_branch)),
            }
        )
    }
}

/// Fold an accumulated list of `(pattern, comp)` bindings around an inner
/// computation, producing a chain of `Comp::Bind` nodes.  Reused by
/// short-circuit lowering to keep conditional-branch binds local.
fn wrap_binds(span: &Option<Span>, binds: Vec<(Pattern, Comp)>, inner: Comp) -> Comp {
    binds
        .into_iter()
        .rev()
        .fold(inner, |rest, (pattern, comp)| {
            Comp::with_span(
                *span,
                CompKind::Bind {
                    comp: Box::new(comp),
                    pattern,
                    rest: Box::new(rest),
                },
            )
        })
}

/// If `comp` is `Return(v)`, produce `Force(v)` directly.
/// Otherwise wrap `comp` in a thunk first: `Force(Thunk(comp))`.
fn force_from_comp(span: Option<Span>, comp: Comp) -> Comp {
    let kind = match comp.kind {
        CompKind::Return(v) => CompKind::Force(v),
        _ => CompKind::Force(Val::Thunk(Arc::new(comp))),
    };
    Comp::with_span(span, kind)
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
pub fn elaborate(ast: &[Ast], bindings: HashSet<String>) -> Comp {
    let comp = Elaborator::new_with_bindings(bindings).stmts(ast);
    if std::env::var("RAL_DUMP_IR").is_ok() {
        eprintln!("{comp:#?}");
    }
    comp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    #[test]
    fn tilde_path_command_head_elaborates_to_exec() {
        let ast = parse("~/.local/bin/claude update").expect("parse");
        let comp = elaborate(&ast, HashSet::new());
        let CompKind::Exec { name, args, .. } = &comp.kind else {
            panic!("expected exec, got {:?}", comp.kind);
        };
        assert_eq!(
            name,
            &ExecName::TildePath(crate::path::tilde::TildePath {
                user: None,
                suffix: Some("/.local/bin/claude".into()),
            })
        );
        assert_eq!(args, &vec![Val::Literal("update".into())]);
    }

    #[test]
    fn tilde_path_command_head_without_args_elaborates_to_exec() {
        let ast = parse("~/.local/bin/claude").expect("parse");
        let comp = elaborate(&ast, HashSet::new());
        let CompKind::Exec { name, args, .. } = &comp.kind else {
            panic!("expected exec, got {:?}", comp.kind);
        };
        assert_eq!(
            name,
            &ExecName::TildePath(crate::path::tilde::TildePath {
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
        let CompKind::Exec { name, args, .. } = &comp.kind else {
            panic!("expected exec, got {:?}", comp.kind);
        };
        assert_eq!(name, &ExecName::Path("./script".into()));
        assert!(args.is_empty());
    }

    #[test]
    fn external_name_head_elaborates_to_external_exec() {
        let ast = parse("^git status").expect("parse");
        let comp = elaborate(&ast, HashSet::new());
        let CompKind::Exec {
            name,
            args,
            external_only,
            ..
        } = &comp.kind
        else {
            panic!("expected exec, got {:?}", comp.kind);
        };
        assert_eq!(name, &ExecName::Bare("git".into()));
        assert_eq!(args, &vec![Val::Literal("status".into())]);
        assert!(*external_only);
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
        let CompKind::App { head, args, .. } = &comp.kind else {
            panic!("expected app, got {:?}", comp.kind);
        };
        let CompKind::Return(Val::Variable(name)) = &head.kind else {
            panic!("expected returned-variable head, got {:?}", head.kind);
        };
        assert_eq!(name, "map");
        assert_eq!(
            args,
            &vec![
                Val::Variable("upper".into()),
                Val::List(vec![ValListElem::Single(Val::String("a".into()))]),
            ]
        );
    }
}
