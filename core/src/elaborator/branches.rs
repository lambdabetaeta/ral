use super::{
    Arc, Ast, CaseArm, Comp, CompKind, Elaborator, IfBranch, Pattern, Spanned, Val, WithSpan, ast,
    wrap_binds,
};

impl Elaborator {
    /// Nest `if`/`elsif`/`else` into `CompKind::If`.  The first cond always
    /// runs, so it hoists into the caller's `binds`; every later cond gets a
    /// local vector wrapped inside the thunk that guards it.
    pub(super) fn elab_if(
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
    pub(super) fn elab_arm(&mut self, arm: &Spanned<Box<Ast>>) -> Spanned<Val> {
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

    pub(super) fn elab_case_arm(&mut self, arm: &ast::CaseArm) -> CaseArm {
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
    pub(super) fn elab_guarded(&mut self, ast: &Ast) -> Comp {
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
    pub(super) fn elab_isolated(&mut self, ast: &Ast) -> Comp {
        let mut stage_binds = Vec::new();
        let body = self.elab_expr(ast, &mut stage_binds);
        wrap_binds(self.current_span, stage_binds, body)
    }

    /// Desugar `a && b` / `a || b` into an `If`.  The RHS runs only
    /// conditionally, so it lowers in an isolated `binds` vector.
    pub(super) fn lower_short_circuit(
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
pub(super) enum Junction {
    And,
    Or,
}
