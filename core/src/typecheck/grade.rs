//! The grade rules: where a command, `F^w Unit`, meets a value producer,
//! `F^p A`.  The bind rule captures a command and binds its output.  A block
//! handed to a demand of the other kind is adapted — a command is captured
//! where a value is wanted, a `()` producer stands as a command where one is
//! wanted — and the arms of a joining form meet the join they decide between
//! them the same way.  A stdout redirect discharges a command to a value
//! producer.

use super::env::{comp_key, val_key};
use super::error::Reason;
use super::infer::Inferencer;
use super::ty::{CompTy, Grade, Producer, Spine, Ty};
use crate::ir::{Comp, CompKind, Val};
use crate::source::{Span, Spanned, WithSpan};

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
            _ => self.span,
        }
    }
}

/// How a block meets a demand of the other producer kind.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Adaptation {
    /// A command where a value was demanded: `cap`, then `decode`.
    Capture,
    /// A `()` producer where a command was demanded: identity at runtime.
    Upcast,
}

impl Adaptation {
    /// What an adapted block is read as producing, past its parameters.
    fn produces(self) -> CompTy {
        match self {
            Self::Capture => CompTy::pure(Ty::String),
            Self::Upcast => CompTy::command(),
        }
    }
}

/// What an arm's producer asks of its join; the greatest wins.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Verdict {
    /// Nothing yet: the arms simply agree.
    Open,
    /// A command, which a `()` producer stands as.
    Command,
    /// A value other than `()`, which every command arm is captured to.
    Value,
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

    /// A stdout redirect takes a command's output, so what is left is a value
    /// producer: `M > f : F^p A`.
    pub(super) fn discharge(&self, cty: &CompTy) -> CompTy {
        self.map_producer(cty, |producer| CompTy::pure(producer.ty))
    }

    /// The bind rule: what a binder's pattern reaches from its RHS's type.  A
    /// `Fun` RHS is a lambda, so the whole arrow is thunked; a command is
    /// captured, and the binder takes its output; a value producer's value is
    /// bound; a producer of unknown grade is made a value producer — a `let`
    /// is a demand for a value, the one defaulting in the system.  Shared by
    /// `Bind` and `Phrase::Define`.
    pub(super) fn rhs_bound_ty(&mut self, rhs: &Comp, cty: CompTy) -> Ty {
        match self.ctx.unifier.resolve_comp_ty(&cty) {
            CompTy::Fun(..) => Ty::Thunk(Box::new(cty)),
            CompTy::Return(Grade::Output, unit) => {
                debug_assert!(self.is_unit(&unit));
                self.ctx.captured.insert(comp_key(rhs));
                Ty::String
            }
            CompTy::Return(Grade::Value, ty) => *ty,
            CompTy::Return(Grade::Var(grade), ty) => {
                self.ctx.unifier.bind_grade(grade, Grade::Value);
                *ty
            }
            CompTy::Var(_) => {
                let ty = self.ctx.unifier.fresh_ty();
                self.ctx
                    .unify_comp_ty(&cty, &CompTy::pure(ty.clone()), Reason::ReturnShape);
                ty
            }
        }
    }

    /// How a block of type `given` meets the demand `wanted`, when the two
    /// are producers of one arity of different kinds.  `None` when nothing
    /// adapts.
    pub(super) fn adapt(&self, given: &CompTy, wanted: &CompTy) -> Option<Adaptation> {
        let (given, wanted) = (self.spine(given), self.spine(wanted));
        if given.arity() != wanted.arity() {
            return None;
        }
        let (given, wanted) = (given.producer()?, wanted.producer()?);
        match (given.grade, wanted.grade) {
            (Grade::Output, Grade::Value) => Some(Adaptation::Capture),
            (Grade::Value, Grade::Output) if self.is_unit(&given.ty) => Some(Adaptation::Upcast),
            _ => None,
        }
    }

    /// `block`, of type `given`, read as `how` says: a captured block is
    /// marked at its arity, and the type it is read at is unified with the
    /// demand `wanted`.  A failure is reported between what the two produce
    /// past the parameters they agree on — the block as written, and the
    /// demand.
    pub(super) fn coerce(
        &mut self,
        block: &Val,
        given: &CompTy,
        how: Adaptation,
        wanted: &CompTy,
        why: &Reason,
    ) {
        let given = self.spine(given);
        if how == Adaptation::Capture {
            self.capture_block(block, given.arity());
        }
        let shown = (self.spine(wanted).tail, given.tail.clone());
        let read_at = given.over(how.produces());
        self.ctx
            .unify_comp_ty_as(&read_at, wanted, shown, why.clone());
    }

    /// Mark a block for capture: a literal one by its body under exactly
    /// `arity` lambdas; any other by itself, η-wrapped at that arity — a
    /// value in hand, or a literal whose arrows are not all written, `{ !$f }`,
    /// whose body the capture frame would otherwise part from its argument.
    fn capture_block(&mut self, block: &Val, arity: usize) {
        if let Val::Thunk(comp) = block
            && let Some(body) = Self::body_under(comp.shape(), arity)
        {
            self.ctx.captured.insert(comp_key(body));
        } else {
            self.ctx.captured_vals.insert(val_key(block), arity);
        }
    }

    /// `comp` under exactly `arity` lambdas.
    fn body_under(comp: &Comp, arity: usize) -> Option<&Comp> {
        match (&comp.item, arity) {
            (_, 0) => Some(comp),
            (CompKind::Lam { body, .. }, _) => Self::body_under(body, arity - 1),
            _ => None,
        }
    }

    /// The arms of a joining form: exactly one runs, so they share one type.
    ///
    /// Two passes.  Each arm is first checked against its own fresh result,
    /// past the parameters its form hands it; then the results decide the
    /// join ([`Self::join_target`]), and each arm meets it as a block meets
    /// a demand.
    pub(super) fn join_arms(&mut self, arms: &[JoinArm<'_>], why: &Reason) -> CompTy {
        let results: Vec<CompTy> = arms
            .iter()
            .map(|arm| {
                let rho = self.ctx.unifier.fresh_comp_ty();
                self.check_arm(arm, &arm.shaped(rho.clone()));
                rho
            })
            .collect();
        let target = self.join_target(&results);
        // The value arms fix the target first, so a captured arm that still
        // disagrees is reported as the command it is against that value.
        let mut ordered: Vec<_> = arms.iter().zip(&results).collect();
        ordered.sort_by_key(|(_, rho)| self.verdict(rho) == Verdict::Command);
        for (arm, rho) in ordered {
            self.with_span(arm.result_span(), |this| {
                let (shape, demand) = (arm.shaped(rho.clone()), arm.shaped(target.clone()));
                match this.adapt(&shape, &demand) {
                    Some(how) => this.coerce(arm.value, &shape, how, &demand, why),
                    None => this.unify_arm(rho, &target, why),
                }
            });
        }
        target
    }

    fn verdict(&self, rho: &CompTy) -> Verdict {
        match self.ctx.unifier.resolve_comp_ty(rho) {
            CompTy::Return(Grade::Value, a) if !self.is_unit(&a) => Verdict::Value,
            CompTy::Return(Grade::Output, _) => Verdict::Command,
            _ => Verdict::Open,
        }
    }

    /// What a join settles on: the greatest of its arms' verdicts.
    fn join_target(&mut self, results: &[CompTy]) -> CompTy {
        let verdict = results.iter().map(|rho| self.verdict(rho)).max();
        match verdict.unwrap_or(Verdict::Open) {
            Verdict::Value => CompTy::pure(self.ctx.unifier.fresh_ty()),
            Verdict::Command => CompTy::command(),
            Verdict::Open => self.ctx.unifier.fresh_comp_ty(),
        }
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

    /// An arm's result against the join's: the values, where both produce
    /// one at the same grade, so that a disagreement is between two values
    /// rather than between two computations.
    fn unify_arm(&mut self, got: &CompTy, target: &CompTy, why: &Reason) {
        match (
            self.ctx.unifier.resolve_comp_ty(got),
            self.ctx.unifier.resolve_comp_ty(target),
        ) {
            (CompTy::Return(g, a), CompTy::Return(h, b)) if g == h => {
                self.ctx.unify_ty(&b, &a, why.clone());
            }
            _ => self.ctx.unify_comp_ty(target, got, why.clone()),
        }
    }
}
