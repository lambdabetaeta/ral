//! Fresh producer grades, source-order joins, and capture at `let` only.

use super::error::Reason;
use super::infer::Inferencer;
use crate::ir::Val;
use crate::source::{Span, Spanned, WithSpan};
use crate::ty::{CompTy, Grade, Producer, Spine, Ty};

/// What an arm of a joining form is checked as: the value written, the
/// parameter types its form hands it, and the reason its own shape is held
/// to.  The form's arms are then joined under the form's reason.
pub(super) struct JoinArm<'a> {
    value: &'a Val,
    span: Option<Span>,
    params: Vec<Ty>,
    why: Reason,
}

impl<'a> JoinArm<'a> {
    pub(super) fn new(value: &'a Spanned<Val>, params: Vec<Ty>, why: Reason) -> Self {
        Self {
            value: &value.item,
            span: value.span,
            params,
            why,
        }
    }

    /// An arm the form carries without a span of its own.
    pub(super) fn in_hand(value: &'a Val, params: Vec<Ty>, why: Reason) -> Self {
        Self {
            value,
            span: None,
            params,
            why,
        }
    }

    /// `tail` under the parameters the form hands this arm.
    fn shaped(&self, tail: CompTy) -> CompTy {
        CompTy::arrows(self.params.iter().cloned(), tail)
    }

    /// The span the arm's result comes from: its tail, through lambda bodies
    /// and the rest of each bind, or the arm itself when it is in hand.
    pub(super) fn result_span(&self) -> Option<Span> {
        match self.value {
            Val::Thunk(comp) => Inferencer::result_span(comp.shape()),
            Val::Protected { value, .. } => match value.as_ref() {
                Val::Thunk(comp) => Inferencer::result_span(comp.shape()),
                _ => self.span,
            },
            _ => self.span,
        }
    }
}

/// How a right-hand side is bound.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Bound {
    Ran,
    Thunked,
    Captured,
}

impl Inferencer<'_> {
    pub(super) fn is_unit(&self, ty: &Ty) -> bool {
        matches!(self.ctx.unifier.resolve_ty(ty), Ty::Unit)
    }

    /// `cty`'s curry spine, resolved as far as it goes.
    pub(super) fn spine(&self, cty: &CompTy) -> Spine {
        let mut params = Vec::new();
        let mut tail = self.ctx.unifier.resolve_comp_ty(cty);
        while let CompTy::Fun(param, body) = tail {
            params.push(*param);
            tail = self.ctx.unifier.resolve_comp_ty(&body);
        }
        Spine { params, tail }
    }

    /// `cty` with what it produces past its parameters rebuilt by `f`; a
    /// variable there is left as it is.
    pub(super) fn map_producer(&self, cty: &CompTy, f: impl FnOnce(Producer) -> CompTy) -> CompTy {
        let spine = self.spine(cty);
        match spine.producer() {
            Some(producer) => spine.over(f(producer)),
            None => cty.clone(),
        }
    }

    /// Redirects scope a ready computation, not a future application.
    pub(super) fn redirected(&mut self, cty: &CompTy, stdout: bool) -> CompTy {
        let producer = self.force_return_shape(cty, Reason::RedirectShape);
        if stdout {
            self.produces(producer.ty)
        } else {
            producer.into()
        }
    }

    /// The bind rule: what a binder's pattern reaches from its RHS's type.  A
    /// `Fun` RHS is a function, bound unrun as its thunk (Let-fun); a command is
    /// captured, and the binder takes its output; a value producer's value is
    /// bound; a producer of unknown grade is made a value producer — a `let`
    /// is a demand for a value, the one grade defaulting.  Shared by
    /// `Bind` and `Phrase::Define`.
    pub(super) fn rhs_bound_ty(&mut self, cty: CompTy) -> (Ty, Bound) {
        match self.ctx.unifier.resolve_comp_ty(&cty) {
            CompTy::Fun(..) => (Ty::Thunk(Box::new(cty)), Bound::Thunked),
            CompTy::Return(Grade::Output, unit) => {
                debug_assert!(self.is_unit(&unit));
                (Ty::String, Bound::Captured)
            }
            CompTy::Return(Grade::Value, ty) => (*ty, Bound::Ran),
            CompTy::Return(Grade::Var(grade), ty) => {
                self.ctx.unifier.bind_grade(grade, Grade::Value);
                (*ty, Bound::Ran)
            }
            CompTy::Var(_) => {
                let ty = self.ctx.unifier.fresh_ty();
                self.ctx
                    .unify_comp_ty(&cty, &CompTy::pure(ty.clone()), Reason::ReturnShape);
                (ty, Bound::Ran)
            }
        }
    }

    pub(super) fn produces(&mut self, ty: Ty) -> CompTy {
        CompTy::Return(self.ctx.unifier.fresh_grade(), Box::new(ty))
    }

    /// Check independently, then join in source order.
    pub(super) fn join_arms(&mut self, arms: &[JoinArm<'_>], why: &Reason) -> CompTy {
        let results: Vec<CompTy> = arms
            .iter()
            .map(|arm| {
                let rho = self.ctx.unifier.fresh_comp_ty();
                self.check_arm(arm, &arm.shaped(rho.clone()));
                rho
            })
            .collect();
        let target = self.ctx.unifier.fresh_comp_ty();
        for (arm, rho) in arms.iter().zip(&results) {
            self.with_span(arm.result_span(), |this| {
                this.ctx.unify_comp_ty(&target, rho, why.clone());
            });
        }
        target
    }

    /// `arm` against the computation type `expected`: a literal block is
    /// inferred in a scope of its own, pushing `expected` inwards as a
    /// lambda's parameter; a thunk in hand is unified.  A mismatch is blamed
    /// on the arm's own span.
    fn check_arm(&mut self, arm: &JoinArm<'_>, expected: &CompTy) {
        self.with_span(arm.span, |this| match arm.value {
            Val::Thunk(comp) => {
                let got = this.with_scope(|this| this.check_comp(comp.shape(), expected));
                this.with_span(arm.result_span(), |this| {
                    this.ctx.unify_comp_ty(expected, &got, arm.why.clone());
                });
            }
            other => {
                let ty = this.infer_val(other);
                let expected = Ty::Thunk(Box::new(expected.clone()));
                this.ctx.unify_ty(&ty, &expected, arm.why.clone());
            }
        });
    }
}
