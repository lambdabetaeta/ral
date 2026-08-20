//! The one join this module still defers: an arm-result merge under the
//! subsumption instance `Value Unit ⊑ Bytes` (plan §4.3). `if`, `?`, `case`,
//! and `try` all funnel their arms through [`InferCtx::join_arm_results`]
//! rather than casing on the unifier's state at visit time: an emission that
//! is already determined applies immediately — sound because a route only
//! ever moves `Var → ground`, never back, so an early conclusion can't be
//! invalidated later. What's left undetermined is stored as an [`ArmResults`]
//! and revisited at [`InferCtx::solve_at_boundary`], which every
//! scheme-producing boundary calls with its environment — each boundary
//! collapses only the constraints whose variables it is about to quantify —
//! and at the terminal [`InferCtx::solve_and_finalize`], which collapses
//! everything.
//!
//! The join itself: `Value A` beside `Value B` unifies `A` and `B`; `Bytes`
//! beside `Bytes` stays `Bytes`; `Value Unit` beside `Bytes` coerces to the
//! byte side; `Value A` for non-`Unit` `A` beside `Bytes` is the conduit
//! mismatch; a wholly open join defers to its owning boundary; divergence is
//! neutral until another arm determines the route.

use super::env::{InferCtx, TyEnv};
use super::error::Reason;
use super::generalize::env_free_vars;
use super::ty::{CompTy, PayloadRoute, PayloadVar, Ty};
use crate::source::Span;
use std::collections::HashSet;

/// A deferred arm-result join, carrying the provenance of the site that
/// raised it so a failure surfaced at [`InferCtx::solve_and_finalize`] still
/// blames the constraint's own position and [`Reason`], not whatever `pos`
/// happens to be current when the worklist runs.
///
/// `pub(super)` only so [`InferCtx`]'s store field in `env.rs` can name the
/// element type; nothing outside `typecheck` sees it, and nothing outside
/// this file ever cases on one.
pub(super) struct ArmResults {
    result: PayloadRoute,
    value: Ty,
    arms: Vec<(PayloadRoute, Ty)>,
    pos: Option<Span>,
    why: Reason,
}

/// The value-side twin of an arm join's reason: the same form, but explained
/// as a disagreement about what the payload *is* rather than about where it
/// lives. Each of the four join sites has one; any other reason, having no
/// route side to be mistaken for, stands as it is.
fn value_side(why: &Reason) -> Reason {
    match why {
        Reason::IfBranches => Reason::IfBranchValues,
        Reason::ChainBranches => Reason::ChainBranchValues,
        Reason::CaseArms => Reason::CaseArmValues,
        Reason::TryArms => Reason::TryArmValues,
        other => other.clone(),
    }
}

impl InferCtx {
    /// The payload join at the heart of every arm merge: which side carries
    /// the arms' payload, under the one subsumption instance `Value Unit ⊑
    /// Bytes`. Some arm routed `Bytes` pulls the join onto the byte side; no
    /// byte arm and every arm routed `Value` pulls it onto the value side;
    /// any arm still open defers — even beside a ground `Value`-at-non-`Unit`
    /// arm, because the open arm may yet ground `Bytes`, and that verdict
    /// (the conduit mismatch) must be the join's own, not foreclosed by
    /// pinning the open arm early.
    pub(super) fn join_arm_results(
        &mut self,
        arms: Vec<(PayloadRoute, Ty)>,
        why: Reason,
    ) -> (PayloadRoute, Ty) {
        if arms.is_empty() {
            return (PayloadRoute::Value, self.unifier.fresh_ty());
        }
        if let Some(concluded) = self.conclude_arm_results(&arms, &why) {
            return concluded;
        }
        let result = self.unifier.fresh_route();
        let value = self.unifier.fresh_ty();
        self.route_constraints.push(ArmResults {
            result,
            value: value.clone(),
            arms,
            pos: self.pos,
            why,
        });
        (result, value)
    }

    /// Terminal drain, where nothing encloses the store — the end of a check
    /// and the empty-environment scheme builders. Every residual constraint
    /// collapses; none survives.
    pub(super) fn solve_and_finalize(&mut self) {
        self.solve(None);
        debug_assert!(
            self.route_constraints.is_empty(),
            "a constraint outlived the terminal drain"
        );
    }

    /// Boundary drain, run at every scheme-producing point. Solves only the
    /// constraints this generalisation owns — those touching a route
    /// variable not free in `env`, which is exactly a variable `generalize`
    /// is about to quantify, and quantification is what a constraint must
    /// not outlive. A constraint whose every variable is still free in the
    /// environment belongs to an enclosing binding and is left *entirely*
    /// untouched for that binding's own boundary: not collapsed, and not
    /// retried either — a conclusion's side effects discipline arms that are
    /// still under inference elsewhere, so running it at a boundary a
    /// syntactic accident placed (any inner `let`, the elaborator's hoisted
    /// binds included) would pin an enclosing group's still-open arms and
    /// move their errors.
    pub(super) fn solve_at_boundary(&mut self, env: &TyEnv) {
        self.solve(Some(env));
    }

    /// Set aside what this boundary does not own, then collapse the residue
    /// one constraint at a time, re-running the worklist between each — every
    /// `ArmResults` writes ground state on collapse, so a write that
    /// determines a sibling reaches that sibling's own rule instead of being
    /// raced by a bare equate. Each pass retires at least one constraint, so
    /// the loop terminates.
    fn solve(&mut self, env: Option<&TyEnv>) {
        let mut kept = Vec::new();
        if let Some(env) = env
            && !self.route_constraints.is_empty()
        {
            let env_routes = env_free_vars(&mut self.unifier, env).routes;
            for c in std::mem::take(&mut self.route_constraints) {
                if self.owned_by_env(&c, &env_routes) {
                    kept.push(c);
                } else {
                    self.route_constraints.push(c);
                }
            }
        }
        self.retry_to_quiescence();
        while let Some(c) = self.route_constraints.pop() {
            self.collapse_ground(c);
            self.retry_to_quiescence();
        }
        debug_assert!(
            self.route_constraints.is_empty(),
            "no collapse emits constraints; one here would outlive its boundary"
        );
        self.route_constraints = kept;
    }

    /// Does every open route variable this constraint could write still
    /// occur in the environment? Then an enclosing binding owns the
    /// constraint and this boundary must not collapse it. Only writable
    /// positions count: the result and every arm's own route — never an
    /// arm's value, which the join never grounds on its own.
    fn owned_by_env(&mut self, c: &ArmResults, env_routes: &HashSet<PayloadVar>) -> bool {
        self.env_owned(c.result, env_routes)
            && c.arms
                .iter()
                .all(|(route, _)| self.env_owned(*route, env_routes))
    }

    fn env_owned(&mut self, route: PayloadRoute, env_routes: &HashSet<PayloadVar>) -> bool {
        match self.unifier.resolve_route(&route) {
            PayloadRoute::Var(v) => env_routes.contains(&v),
            PayloadRoute::Value | PayloadRoute::Bytes => true,
        }
    }

    // ── conclusion rules, shared between emission and the worklist ──

    /// Applies the arm-results conclusion's side effects — pinning open
    /// routes, forcing the byte-side `Return`-shape mismatch, tying values —
    /// the moment they're determined, and returns the settled pair; `None`
    /// while no arm has yet said which side the join lands on.
    fn conclude_arm_results(
        &mut self,
        arms: &[(PayloadRoute, Ty)],
        why: &Reason,
    ) -> Option<(PayloadRoute, Ty)> {
        let any_bytes = arms
            .iter()
            .any(|(route, _)| matches!(self.unifier.resolve_route(route), PayloadRoute::Bytes));
        if any_bytes {
            return Some(self.conclude_byte_side(arms, why));
        }

        let any_open = arms
            .iter()
            .any(|(route, _)| matches!(self.unifier.resolve_route(route), PayloadRoute::Var(_)));
        if any_open {
            return None;
        }
        Some(self.conclude_value_side(arms, why))
    }

    /// The byte side of an arm join.  WF-2 admits exactly one byte-routed
    /// computation, so landing here is structural: each arm unifies with
    /// [`CompTy::bytes`] whole — route and value together, so a sibling
    /// grounding that reached an arm's route without touching its value
    /// still ties the value here — under the single subsumption
    /// `Value Unit ⊑ Bytes`, which admits the empty branch as it stands.
    ///
    /// The subsumed arm keeps its `Value` route, but every arm's value is
    /// `Unit` here in *every* solution, so that is an equation to impose
    /// rather than a question to ask: an arm whose value is still open is
    /// not thereby a payload-carrying arm. What cannot meet the equation is
    /// the conduit mismatch, and it reads truest as the whole computation
    /// clashing with the byte-routed one — route diff and all.
    fn conclude_byte_side(
        &mut self,
        arms: &[(PayloadRoute, Ty)],
        why: &Reason,
    ) -> (PayloadRoute, Ty) {
        for (route, ty) in arms {
            if matches!(self.unifier.resolve_route(route), PayloadRoute::Value)
                && self.unifier.unify_ty(ty, &Ty::Unit).is_ok()
            {
                continue;
            }
            let arm = CompTy::Return(*route, Box::new(ty.clone()));
            self.unify_comp_ty(&CompTy::bytes(), &arm, why.clone());
        }
        (PayloadRoute::Bytes, Ty::Unit)
    }

    /// The value side of an arm join: open routes pin `Value` and the values
    /// unify into one payload type.
    ///
    /// Those values unify under the join's *value-side* reason. Every arm
    /// lands here already routed `Value`, so nothing about where the payload
    /// lives is in dispute, and the route-side reason — which counsels a
    /// decoder — would name a fault the program does not have.
    fn conclude_value_side(
        &mut self,
        arms: &[(PayloadRoute, Ty)],
        why: &Reason,
    ) -> (PayloadRoute, Ty) {
        for (route, _) in arms {
            if matches!(self.unifier.resolve_route(route), PayloadRoute::Var(_)) {
                self.unify_route(route, &PayloadRoute::Value, Reason::RoutePin);
            }
        }
        let values_agree = value_side(why);
        let mut values = arms.iter().map(|(_, ty)| ty.clone());
        let first = values.next().expect("a join always has at least one arm");
        for ty in values {
            self.unify_ty(&first, &ty, values_agree.clone());
        }
        (PayloadRoute::Value, first)
    }

    // ── the worklist: retry stored constraints, then collapse the residue ──

    /// Retry until a full pass retires nothing — a conclusion may determine
    /// a sibling constraint. Terminates because the route lattice has height
    /// one and each conclusion drops its own constraint.
    fn retry_to_quiescence(&mut self) {
        loop {
            let pending = std::mem::take(&mut self.route_constraints);
            let before = pending.len();
            let kept: Vec<ArmResults> = pending.into_iter().filter_map(|c| self.retry(c)).collect();
            debug_assert!(
                self.route_constraints.is_empty(),
                "no conclusion rule emits constraints; a push here would be lost"
            );
            self.route_constraints = kept;
            if self.route_constraints.len() == before {
                return;
            }
        }
    }

    /// One quiescence pass over a single constraint: `Some` keeps it stored
    /// unchanged, `None` means it concluded and applied its side effects.
    ///
    /// The constraint's own `pos` is installed unconditionally, `None`
    /// included. A deferred constraint's diagnostic belongs at the site that
    /// deferred it, and the ambient position during a quiescence sweep is some
    /// unrelated form's — so falling back to it, rather than to no position at
    /// all, would point the error somewhere the reader never wrote.
    fn retry(&mut self, c: ArmResults) -> Option<ArmResults> {
        let saved = std::mem::replace(&mut self.pos, c.pos);
        let out = match self.conclude_arm_results(&c.arms, &c.why) {
            Some(conclusion) => {
                let ArmResults {
                    result, value, why, ..
                } = c;
                self.apply_conclusion(result, value, conclusion, why);
                None
            }
            None => Some(c),
        };
        self.pos = saved;
        out
    }

    /// Apply a settled conclusion to the join's own pair — as one `Return`,
    /// so the route never travels detached from the value it is paired with.
    fn apply_conclusion(
        &mut self,
        result: PayloadRoute,
        value: Ty,
        (route, ty): (PayloadRoute, Ty),
        why: Reason,
    ) {
        self.unify_comp_ty(
            &CompTy::Return(result, Box::new(value)),
            &CompTy::Return(route, Box::new(ty)),
            why,
        );
    }

    /// Collapse a residual `ArmResults`, directed by where its own `result`
    /// route stands. Grounded from outside — a downstream consumer pinned
    /// the form's result — the arms land on that side with the full
    /// protocol, not a bare equation. Still open: a `Value`-routed arm at a
    /// *solved* non-`Unit` type has, by now, spent every chance to subsume
    /// onto a byte side, so it decides the value side and pins the arms
    /// still open. An arm whose value type is merely not yet known is not
    /// that evidence — reading absence as payload would make the verdict
    /// turn on how much of the store a boundary happened to have solved,
    /// so that an equation added elsewhere could make a program typecheck.
    /// Otherwise every open route is folded into one class and every value
    /// tied to the join's own `value`, so the whole join stays one variable
    /// a later grounding can still move. Writes ground state; the caller
    /// re-runs the worklist after each.
    ///
    /// Installs `c.pos` unconditionally, as [`Self::retry`] does and for the
    /// same reason.
    fn collapse_ground(&mut self, c: ArmResults) {
        let saved = std::mem::replace(&mut self.pos, c.pos);
        let ArmResults {
            result,
            value,
            arms,
            why,
            ..
        } = c;
        let concluded = match self.unifier.resolve_route(&result) {
            PayloadRoute::Bytes => Some(self.conclude_byte_side(&arms, &why)),
            PayloadRoute::Value => Some(self.conclude_value_side(&arms, &why)),
            PayloadRoute::Var(_)
                if arms.iter().any(|(route, ty)| {
                    matches!(self.unifier.resolve_route(route), PayloadRoute::Value)
                        && !matches!(self.unifier.resolve_ty(ty), Ty::Unit | Ty::Var(_))
                }) =>
            {
                Some(self.conclude_value_side(&arms, &why))
            }
            PayloadRoute::Var(_) => {
                let open: Vec<PayloadRoute> = arms
                    .iter()
                    .map(|(route, _)| *route)
                    .filter(|r| matches!(self.unifier.resolve_route(r), PayloadRoute::Var(_)))
                    .collect();
                let mut open = open.into_iter();
                if let Some(first) = open.next() {
                    for other in open {
                        self.unify_route(&first, &other, why.clone());
                    }
                    self.unify_route(&result, &first, why.clone());
                }
                for (_, ty) in &arms {
                    self.unify_ty(ty, &value, why.clone());
                }
                None
            }
        };
        if let Some(conclusion) = concluded {
            self.apply_conclusion(result, value, conclusion, why);
        }
        self.pos = saved;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typecheck::Scheme;

    fn is_open(r: PayloadRoute) -> bool {
        matches!(r, PayloadRoute::Var(_))
    }

    // ── ArmResults ──

    #[test]
    fn arm_results_empty_is_value_and_fresh_value() {
        let mut ctx = InferCtx::new();
        let (result, value) = ctx.join_arm_results(vec![], Reason::CaseArms);
        assert_eq!(result, PayloadRoute::Value);
        assert!(matches!(ctx.unifier.resolve_ty(&value), Ty::Var(_)));
    }

    #[test]
    fn arm_results_byte_side_pins_opens_and_ties_values_to_unit() {
        let mut ctx = InferCtx::new();
        let open_route = ctx.unifier.fresh_route();
        let open_value = ctx.unifier.fresh_ty();
        let arms = vec![
            (PayloadRoute::Bytes, Ty::Unit),
            (open_route, open_value.clone()),
        ];
        let (result, value) = ctx.join_arm_results(arms, Reason::CaseArms);
        assert_eq!(result, PayloadRoute::Bytes);
        assert_eq!(value, Ty::Unit);
        assert_eq!(ctx.unifier.resolve_route(&open_route), PayloadRoute::Bytes);
        assert_eq!(ctx.unifier.resolve_ty(&open_value), Ty::Unit);
        assert!(ctx.route_constraints.is_empty());
        assert!(ctx.errors.is_empty());
    }

    #[test]
    fn arm_results_byte_side_reports_value_nonunit_as_return_mismatch() {
        let mut ctx = InferCtx::new();
        let arms = vec![
            (PayloadRoute::Bytes, Ty::Unit),
            (PayloadRoute::Value, Ty::Int),
        ];
        let (result, value) = ctx.join_arm_results(arms, Reason::CaseArms);
        assert_eq!(result, PayloadRoute::Bytes);
        assert_eq!(value, Ty::Unit);
        assert_eq!(
            ctx.errors.len(),
            1,
            "the Value-at-Int arm must report exactly once"
        );
    }

    /// A ground `Value`-at-non-`Unit` arm beside a still-open arm decides
    /// nothing yet — the open arm may ground `Bytes`, and the conduit
    /// verdict must wait for it — so the join defers, and only the boundary
    /// pins the value side.
    #[test]
    fn arm_results_ground_payload_beside_open_arm_defers_then_pins_value_side() {
        let mut ctx = InferCtx::new();
        let open_route = ctx.unifier.fresh_route();
        let open_value = ctx.unifier.fresh_ty();
        let arms = vec![
            (PayloadRoute::Value, Ty::Int),
            (open_route, open_value.clone()),
        ];
        let (result, value) = ctx.join_arm_results(arms, Reason::CaseArms);
        assert_eq!(ctx.route_constraints.len(), 1, "the open arm must defer");
        assert!(is_open(ctx.unifier.resolve_route(&result)));

        ctx.solve_and_finalize();

        assert_eq!(ctx.unifier.resolve_route(&result), PayloadRoute::Value);
        assert_eq!(ctx.unifier.resolve_ty(&value), Ty::Int);
        assert_eq!(ctx.unifier.resolve_route(&open_route), PayloadRoute::Value);
        assert_eq!(ctx.unifier.resolve_ty(&open_value), Ty::Int);
        assert!(ctx.errors.is_empty());
    }

    /// The same start, but the open arm grounds `Bytes` before the boundary:
    /// the join lands on the byte side and the ground `Value`-at-`Int` arm is
    /// the conduit mismatch, reported under the join's own reason — not an
    /// early value-side pin surfacing wherever the `Bytes` grounding happens.
    #[test]
    fn arm_results_ground_payload_beside_late_byte_arm_is_the_joins_own_mismatch() {
        let mut ctx = InferCtx::new();
        let open_route = ctx.unifier.fresh_route();
        let arms = vec![(PayloadRoute::Value, Ty::Int), (open_route, Ty::Unit)];
        let (result, value) = ctx.join_arm_results(arms, Reason::CaseArms);
        assert_eq!(ctx.route_constraints.len(), 1, "the open arm must defer");

        ctx.unify_route(&open_route, &PayloadRoute::Bytes, Reason::RoutePin);
        assert!(ctx.errors.is_empty(), "the grounding site stays blameless");
        ctx.solve_and_finalize();

        assert_eq!(ctx.unifier.resolve_route(&result), PayloadRoute::Bytes);
        assert_eq!(ctx.unifier.resolve_ty(&value), Ty::Unit);
        assert_eq!(ctx.errors.len(), 1, "one conduit mismatch, at the join");
        assert!(matches!(ctx.errors[0].reason, Some(Reason::CaseArms)));
    }

    #[test]
    fn arm_results_undetermined_defers_and_does_not_unify_values_yet() {
        let mut ctx = InferCtx::new();
        let r1 = ctx.unifier.fresh_route();
        let r2 = ctx.unifier.fresh_route();
        let v1 = ctx.unifier.fresh_ty();
        let v2 = ctx.unifier.fresh_ty();
        let arms = vec![(r1, v1.clone()), (r2, v2.clone())];
        let (result, value) = ctx.join_arm_results(arms, Reason::CaseArms);
        assert_eq!(ctx.route_constraints.len(), 1);
        assert!(is_open(ctx.unifier.resolve_route(&result)));
        assert!(matches!(ctx.unifier.resolve_ty(&value), Ty::Var(_)));

        // The two arm values must still be independent: pinning one to a
        // concrete type must not have already tied the other to anything.
        ctx.unify_ty(&v1, &Ty::Int, Reason::CaseArms);
        assert!(ctx.errors.is_empty());
        assert!(matches!(ctx.unifier.resolve_ty(&v2), Ty::Var(_)));
    }

    // ── deferral buys something ──

    #[test]
    fn deferred_arm_results_reevaluates_on_the_byte_side_and_closes_wf2() {
        let mut ctx = InferCtx::new();
        let r1 = ctx.unifier.fresh_route();
        let r2 = ctx.unifier.fresh_route();
        let v1 = ctx.unifier.fresh_ty();
        let v2 = ctx.unifier.fresh_ty();
        let (result, value) =
            ctx.join_arm_results(vec![(r1, v1.clone()), (r2, v2.clone())], Reason::CaseArms);
        assert_eq!(ctx.route_constraints.len(), 1, "two open arms must defer");

        ctx.unify_route(&r1, &PayloadRoute::Bytes, Reason::RoutePin);
        ctx.solve_and_finalize();

        assert_eq!(ctx.unifier.resolve_route(&result), PayloadRoute::Bytes);
        assert_eq!(ctx.unifier.resolve_ty(&value), Ty::Unit);
        assert_eq!(ctx.unifier.resolve_route(&r2), PayloadRoute::Bytes);
        // WF-2 closed: v2's arm was still open when r1 grounded, and its
        // value is tied to Unit all the same.
        assert_eq!(ctx.unifier.resolve_ty(&v1), Ty::Unit);
        assert_eq!(ctx.unifier.resolve_ty(&v2), Ty::Unit);
    }

    // ── collapse ──

    #[test]
    fn collapse_ties_arm_values_even_when_residue_never_picked_a_side() {
        let mut ctx = InferCtx::new();
        // Both arms Value-at-Unit: the join never determines which side
        // wins, so it rides to collapse untouched.
        let v1 = ctx.unifier.fresh_ty();
        let v2 = ctx.unifier.fresh_ty();
        ctx.unify_ty(&v1, &Ty::Unit, Reason::CaseArms);
        ctx.unify_ty(&v2, &Ty::Unit, Reason::CaseArms);
        let r1 = ctx.unifier.fresh_route();
        let r2 = ctx.unifier.fresh_route();
        let (result, value) =
            ctx.join_arm_results(vec![(r1, v1.clone()), (r2, v2.clone())], Reason::CaseArms);
        assert_eq!(ctx.route_constraints.len(), 1);

        ctx.solve_and_finalize();

        assert!(is_open(ctx.unifier.resolve_route(&result)));
        assert_eq!(
            ctx.unifier.resolve_route(&r1),
            ctx.unifier.resolve_route(&r2)
        );
        assert_eq!(ctx.unifier.resolve_ty(&value), Ty::Unit);
    }

    // ── order-insensitivity ──

    /// An `ArmResults` join over two arms, grounding each arm's route to
    /// `Bytes` independently. Whether the join is emitted before, between,
    /// or after the two groundings, every arm ends up byte-pinned and every
    /// value tied to `Unit`.
    fn arm_results_scenario(order: [u8; 3]) -> (PayloadRoute, Ty, Ty, Ty) {
        let mut ctx = InferCtx::new();
        let r1 = ctx.unifier.fresh_route();
        let r2 = ctx.unifier.fresh_route();
        let v1 = ctx.unifier.fresh_ty();
        let v2 = ctx.unifier.fresh_ty();
        let mut result_target = None;
        let mut value_target = None;
        for step in order {
            match step {
                0 => {
                    let (result, value) = ctx.join_arm_results(
                        vec![(r1, v1.clone()), (r2, v2.clone())],
                        Reason::CaseArms,
                    );
                    result_target = Some(result);
                    value_target = Some(value);
                }
                1 => ctx.unify_route(&r1, &PayloadRoute::Bytes, Reason::RoutePin),
                2 => ctx.unify_route(&r2, &PayloadRoute::Bytes, Reason::RoutePin),
                _ => unreachable!(),
            }
        }
        ctx.solve_and_finalize();
        (
            ctx.unifier
                .resolve_route(&result_target.expect("emitted above")),
            ctx.unifier
                .resolve_ty(&value_target.expect("emitted above")),
            ctx.unifier.resolve_ty(&v1),
            ctx.unifier.resolve_ty(&v2),
        )
    }

    #[test]
    fn order_insensitive_arm_results_across_emission_and_grounding() {
        let permutations: [[u8; 3]; 6] = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        let results = permutations.map(arm_results_scenario);
        for (result, value, v1, v2) in results {
            assert_eq!(result, PayloadRoute::Bytes);
            assert_eq!(value, Ty::Unit);
            assert_eq!(v1, Ty::Unit);
            assert_eq!(v2, Ty::Unit);
        }
    }

    // ── boundaries own their constraints ──

    /// One binding per route, so an environment can be built to mention any
    /// number of a constraint's writable positions: the constraint belongs
    /// to whichever enclosing binding that scheme stands in for.
    fn env_owning(routes: impl IntoIterator<Item = PayloadRoute>) -> TyEnv {
        let mut env = TyEnv::new();
        for (i, route) in routes.into_iter().enumerate() {
            env.bind(
                format!("owner{i}"),
                Scheme::mono(Ty::Thunk(Box::new(CompTy::Return(
                    route,
                    Box::new(Ty::Unit),
                )))),
            );
        }
        env
    }

    /// A boundary must not touch a constraint whose variables all belong to
    /// the environment — neither conclude it (its side effects would pin
    /// arms still under inference elsewhere) nor collapse it. The owning
    /// drain later lands it on the byte side with subsumption intact.
    #[test]
    fn boundary_leaves_env_owned_constraints_untouched() {
        let mut ctx = InferCtx::new();
        let r1 = ctx.unifier.fresh_route();
        let r2 = ctx.unifier.fresh_route();
        let (result, _) =
            ctx.join_arm_results(vec![(r1, Ty::Unit), (r2, Ty::Unit)], Reason::CaseArms);
        let env = env_owning([r1, r2, result]);

        ctx.solve_at_boundary(&env);

        assert_eq!(ctx.route_constraints.len(), 1, "the constraint is kept");
        assert!(is_open(ctx.unifier.resolve_route(&r1)));
        assert!(is_open(ctx.unifier.resolve_route(&r2)));
        assert_ne!(
            ctx.unifier.resolve_route(&r1),
            ctx.unifier.resolve_route(&r2),
            "sibling arms stay independent past a boundary that does not own them"
        );

        // The arms ground apart only afterwards; the owning drain still
        // applies the byte side with the Value-Unit subsumption intact.
        ctx.unify_route(&r1, &PayloadRoute::Bytes, Reason::RoutePin);
        ctx.solve_and_finalize();
        assert_eq!(ctx.unifier.resolve_route(&result), PayloadRoute::Bytes);
        assert_eq!(ctx.unifier.resolve_route(&r2), PayloadRoute::Bytes);
        assert!(ctx.errors.is_empty());
    }

    // ── collapse is directed by a ground result, never through it ──

    /// An `ArmResults` whose result a downstream consumer grounded `Bytes`
    /// lands on the byte side with the full protocol — values tied to `Unit`
    /// — not a bare equation of the open routes.
    #[test]
    fn ground_bytes_result_directs_arm_results_onto_the_byte_side() {
        let mut ctx = InferCtx::new();
        let r1 = ctx.unifier.fresh_route();
        let r2 = ctx.unifier.fresh_route();
        let v1 = ctx.unifier.fresh_ty();
        let v2 = ctx.unifier.fresh_ty();
        let (result, value) =
            ctx.join_arm_results(vec![(r1, v1.clone()), (r2, v2.clone())], Reason::CaseArms);

        ctx.unify_route(&result, &PayloadRoute::Bytes, Reason::RoutePin);
        ctx.solve_and_finalize();

        assert_eq!(ctx.unifier.resolve_route(&r1), PayloadRoute::Bytes);
        assert_eq!(ctx.unifier.resolve_route(&r2), PayloadRoute::Bytes);
        assert_eq!(ctx.unifier.resolve_ty(&v1), Ty::Unit);
        assert_eq!(ctx.unifier.resolve_ty(&v2), Ty::Unit);
        assert_eq!(ctx.unifier.resolve_ty(&value), Ty::Unit);
    }
}
