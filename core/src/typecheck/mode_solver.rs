//! The mode lattice `∅ ⊑ Bytes` needs a *join*; `Unifier::unify_mode` only
//! gives *equality*. This module is the join, deferred: every join site
//! (`Seq`'s channel over its statements, a scope's channel over its arms, the
//! arm-result conduit an `if`/`?`/`case`/`try` agrees on) emits a
//! [`ModeConstraint`] instead of casing on the unifier's state at visit time.
//! An emission that is already determined applies immediately — sound
//! because a mode only ever moves `Var → ground`, never back, so an early
//! conclusion can't be invalidated later. What's left undetermined is stored
//! and revisited at [`InferCtx::solve_at_boundary`], which every
//! scheme-producing boundary calls with its environment — each boundary
//! collapses only the constraints whose variables it is about to quantify —
//! and at the terminal [`InferCtx::solve_and_finalize`], which collapses
//! everything.
//!
//! Every join-shaped case on a mode's groundness lives here. The reads that
//! remain outside — `infer_pipeline`'s byte-tail verdict, `Bind`'s result
//! pin, `consumes_value_arg`, `lift_channels`' tail-shape verdict — inspect
//! settled state to apply their own rules; none of them computes a join.

use super::env::{InferCtx, TyEnv};
use super::error::Reason;
use super::generalize::env_free_vars;
use super::ty::{CompTy, ModeVar, PipeMode, PipeSpec, Ty};
use crate::source::Span;
use std::collections::HashSet;

/// A deferred join, carrying the provenance of the site that raised it so a
/// failure surfaced at [`InferCtx::solve_and_finalize`] still blames the
/// constraint's own position and [`Reason`], not whatever `pos` happens to be
/// current when the worklist runs.
///
/// `pub(super)` only so [`InferCtx`]'s store field in `env.rs` can name the
/// element type; nothing outside `typecheck` sees it, and nothing outside
/// this file ever cases on one.
pub(super) enum ModeConstraint {
    /// `target = ⊔ ends`, bytes-dominant; constrains the target only, never
    /// writes back into an end.
    Join {
        target: PipeMode,
        ends: Vec<PipeMode>,
        pos: Option<Span>,
        why: Reason,
    },
    /// Arms of which only one runs: agree where every end is ground and
    /// equal, free the target where they're ground and disagree, defer
    /// while any end is still open.
    Alt {
        target: PipeMode,
        ends: Vec<PipeMode>,
        pos: Option<Span>,
        why: Reason,
    },
    /// The result-conduit join under the one subsumption instance
    /// `∅@Unit ⊑ Bytes@Unit`.
    ArmResults {
        result: PipeMode,
        value: Ty,
        arms: Vec<(PipeSpec, Ty)>,
        pos: Option<Span>,
        why: Reason,
    },
}

impl ModeConstraint {
    fn pos(&self) -> Option<Span> {
        match self {
            Self::Join { pos, .. } | Self::Alt { pos, .. } | Self::ArmResults { pos, .. } => *pos,
        }
    }
}

/// The verdict of folding an [`Alt`](ModeConstraint::Alt)'s ends: either they
/// agree on a mode, or they don't and the target is left free — the arms'
/// shared stdin is an unknown, not a contradiction.
enum AltVerdict {
    Agree(PipeMode),
    Disagree,
}

impl InferCtx {
    /// Bytes-dominant join over a form's ends. Any end resolving `Bytes`
    /// dominates; all-`None` joins to `None`; exactly one end still open
    /// beside `None`s *is* that end — the identity law `∅ ⊔ μ = μ`, which is
    /// what keeps mode polymorphism (never default it away). Two or more
    /// open ends with no `Bytes` yet defer to [`Self::solve_and_finalize`].
    pub(super) fn join_modes(&mut self, ends: Vec<PipeMode>, why: Reason) -> PipeMode {
        if let Some(mode) = self.conclude_join(&ends) {
            return mode;
        }
        let target = self.unifier.fresh_mode();
        self.mode_constraints.push(ModeConstraint::Join {
            target,
            ends,
            pos: self.pos,
            why,
        });
        target
    }

    /// Alternation over arms of which only one runs. Ground and equal ends
    /// agree; ground and disagreeing ends free the target rather than
    /// contradict (today's leniency: a mismatch here is an unknown for a
    /// downstream stage to pin, not a fatal clash); any end still open
    /// defers.
    pub(super) fn alt_modes(&mut self, ends: Vec<PipeMode>, why: Reason) -> PipeMode {
        match self.conclude_alt(&ends) {
            Some(AltVerdict::Agree(mode)) => mode,
            Some(AltVerdict::Disagree) => self.unifier.fresh_mode(),
            None => {
                let target = self.unifier.fresh_mode();
                self.mode_constraints.push(ModeConstraint::Alt {
                    target,
                    ends,
                    pos: self.pos,
                    why,
                });
                target
            }
        }
    }

    /// The result-mode join at the heart of every arm merge: which conduit
    /// carries the arms' payload, under the one subsumption instance
    /// `∅@Unit ⊑ Bytes@Unit`. Some arm's result ground `Bytes` pulls the join
    /// onto the byte side; no byte arm and every result ground `∅` pulls it
    /// onto the value side; any arm still open defers — even beside a
    /// ground `∅`-at-non-`Unit` arm, because the open arm may yet ground
    /// `Bytes`, and that verdict (the conduit mismatch) must be the join's
    /// own, not foreclosed by pinning the open arm early.
    pub(super) fn join_arm_results(
        &mut self,
        arms: Vec<(PipeSpec, Ty)>,
        why: Reason,
    ) -> (PipeMode, Ty) {
        if arms.is_empty() {
            return (PipeMode::None, self.unifier.fresh_ty());
        }
        if let Some(concluded) = self.conclude_arm_results(&arms, &why) {
            return concluded;
        }
        let result = self.unifier.fresh_mode();
        let value = self.unifier.fresh_ty();
        self.mode_constraints.push(ModeConstraint::ArmResults {
            result,
            value: value.clone(),
            arms,
            pos: self.pos,
            why,
        });
        (result, value)
    }

    /// Terminal drain, where nothing encloses the store — the end of a check
    /// and the empty-environment scheme builders.  Every residual constraint
    /// collapses; none survives.
    pub(super) fn solve_and_finalize(&mut self) {
        self.solve(None);
        debug_assert!(
            self.mode_constraints.is_empty(),
            "a constraint outlived the terminal drain"
        );
    }

    /// Boundary drain, run at every scheme-producing point.  Solves only the
    /// constraints this generalisation owns — those touching a mode variable
    /// not free in `env`, which is exactly a variable `generalize` is about
    /// to quantify, and quantification is what a constraint must not
    /// outlive.  A constraint whose every variable is still free in the
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

    /// Set aside what this boundary does not own, then propagate what it
    /// does to quiescence and collapse the residue.
    ///
    /// Collapse runs in two regimes.  A residue whose verdict is *directed*
    /// by settled state — a `Join`/`Alt` whose target a neighbour grounded,
    /// an `Alt` carrying a ground end, or any `ArmResults`, whose side
    /// effects write modes and types — collapses one constraint at a time,
    /// re-running the worklist between, so a write that determines a sibling
    /// reaches that sibling's own rule instead of being raced by an equate.
    /// What then remains is `Join`s and `Alt`s open at every position, whose
    /// collapse is pure equating — union-find merges, order-free.  Each pass
    /// retires at least one constraint, so the loop terminates.
    fn solve(&mut self, env: Option<&TyEnv>) {
        let mut kept = Vec::new();
        if let Some(env) = env
            && !self.mode_constraints.is_empty()
        {
            let env_modes = env_free_vars(&mut self.unifier, env).modes;
            for c in std::mem::take(&mut self.mode_constraints) {
                if self.owned_by_env(&c, &env_modes) {
                    kept.push(c);
                } else {
                    self.mode_constraints.push(c);
                }
            }
        }
        loop {
            self.retry_to_quiescence();
            if self.mode_constraints.is_empty() {
                break;
            }
            let mut ground_directed = None;
            for c in std::mem::take(&mut self.mode_constraints) {
                if ground_directed.is_none() && self.writes_ground(&c) {
                    ground_directed = Some(c);
                } else {
                    self.mode_constraints.push(c);
                }
            }
            let Some(c) = ground_directed else {
                for c in std::mem::take(&mut self.mode_constraints) {
                    self.collapse_open(c);
                }
                break;
            };
            self.collapse_ground(c);
        }
        debug_assert!(
            self.mode_constraints.is_empty(),
            "no collapse emits constraints; one here would outlive its boundary"
        );
        self.mode_constraints = kept;
    }

    /// Does every open mode variable this constraint could write still occur
    /// in the environment?  Then an enclosing binding owns the constraint and
    /// this boundary must not collapse it.  Only writable positions count:
    /// `Join`/`Alt` write their target and open ends, `ArmResults` writes
    /// results and byte-side outputs — never an arm's input.
    fn owned_by_env(&mut self, c: &ModeConstraint, env_modes: &HashSet<ModeVar>) -> bool {
        match c {
            ModeConstraint::Join { target, ends, .. }
            | ModeConstraint::Alt { target, ends, .. } => {
                self.env_owned(*target, env_modes)
                    && ends.iter().all(|end| self.env_owned(*end, env_modes))
            }
            ModeConstraint::ArmResults { result, arms, .. } => {
                self.env_owned(*result, env_modes)
                    && arms.iter().all(|(spec, _)| {
                        self.env_owned(spec.result, env_modes)
                            && self.env_owned(spec.output, env_modes)
                    })
            }
        }
    }

    fn env_owned(&mut self, mode: PipeMode, env_modes: &HashSet<ModeVar>) -> bool {
        match self.unifier.resolve_mode(&mode) {
            PipeMode::Var(v) => env_modes.contains(&v),
            PipeMode::None | PipeMode::Bytes => true,
        }
    }

    /// Would collapsing this residue write settled state — ground a mode or
    /// unify a type — rather than merely equate open variables?  A residual
    /// `Join`'s ends are all open (a ground `Bytes` end concludes it, ground
    /// `∅` ends drop out), but an `Alt` defers whenever *any* end is open,
    /// so ground ends can ride to its collapse.
    fn writes_ground(&mut self, c: &ModeConstraint) -> bool {
        match c {
            ModeConstraint::Join { target, .. } => {
                !matches!(self.unifier.resolve_mode(target), PipeMode::Var(_))
            }
            ModeConstraint::Alt { target, ends, .. } => {
                !matches!(self.unifier.resolve_mode(target), PipeMode::Var(_))
                    || ends
                        .iter()
                        .any(|end| !matches!(self.unifier.resolve_mode(end), PipeMode::Var(_)))
            }
            ModeConstraint::ArmResults { .. } => true,
        }
    }

    // ── conclusion rules, shared between emission and the worklist ──

    fn conclude_join(&mut self, ends: &[PipeMode]) -> Option<PipeMode> {
        if ends.is_empty() {
            return Some(PipeMode::None);
        }
        let resolved: Vec<PipeMode> = ends.iter().map(|m| self.unifier.resolve_mode(m)).collect();
        if resolved.iter().any(|m| matches!(m, PipeMode::Bytes)) {
            return Some(PipeMode::Bytes);
        }
        let open: Vec<PipeMode> = resolved
            .into_iter()
            .filter(|m| matches!(m, PipeMode::Var(_)))
            .collect();
        match open.as_slice() {
            [] => Some(PipeMode::None),
            [one] => Some(*one),
            _ => None,
        }
    }

    fn conclude_alt(&mut self, ends: &[PipeMode]) -> Option<AltVerdict> {
        let resolved: Vec<PipeMode> = ends.iter().map(|m| self.unifier.resolve_mode(m)).collect();
        let Some(&first) = resolved.first() else {
            return Some(AltVerdict::Disagree);
        };
        // An alternation of one arm *is* that arm, open or not: there is
        // nothing for a later grounding to disagree with.
        if resolved.len() == 1 {
            return Some(AltVerdict::Agree(first));
        }
        if resolved.iter().any(|m| matches!(m, PipeMode::Var(_))) {
            return None;
        }
        if resolved.iter().all(|m| *m == first) {
            Some(AltVerdict::Agree(first))
        } else {
            Some(AltVerdict::Disagree)
        }
    }

    /// Applies the arm-results conclusion's side effects — pinning open
    /// results, forcing the byte-side `Return`-shape mismatch, tying values —
    /// the moment they're determined, and returns the settled pair; `None`
    /// while no arm has yet said which side the join lands on.
    fn conclude_arm_results(
        &mut self,
        arms: &[(PipeSpec, Ty)],
        why: &Reason,
    ) -> Option<(PipeMode, Ty)> {
        let any_bytes = arms
            .iter()
            .any(|(spec, _)| matches!(self.unifier.resolve_mode(&spec.result), PipeMode::Bytes));

        if any_bytes {
            return Some(self.conclude_byte_side(arms, why));
        }

        let any_open = arms
            .iter()
            .any(|(spec, _)| matches!(self.unifier.resolve_mode(&spec.result), PipeMode::Var(_)));
        if any_open {
            return None;
        }
        Some(self.conclude_value_side(arms, why))
    }

    /// The byte side of an arm join: open results pin `Bytes`, every arm's
    /// value ties to `Unit`, and a ground-`∅` arm subsumes only at `Unit` —
    /// otherwise it is the conduit mismatch, reported as the full `Return`
    /// shape.
    fn conclude_byte_side(&mut self, arms: &[(PipeSpec, Ty)], why: &Reason) -> (PipeMode, Ty) {
        for (spec, ty) in arms {
            match self.unifier.resolve_mode(&spec.result) {
                // WF-2 (a byte result returns Unit) is enforced, not assumed:
                // a sibling join may have pinned this arm's result `Bytes`
                // without touching its value, so unify rather than assert.
                // The arm's `output` is its chatter and independent of where
                // its payload rides, so the join leaves it alone.
                PipeMode::Bytes => {
                    self.unify_ty(ty, &Ty::Unit, why.clone());
                }
                PipeMode::Var(_) => {
                    self.unify_mode(&spec.result, &PipeMode::Bytes, Reason::ResultPin);
                    self.unify_ty(ty, &Ty::Unit, why.clone());
                }
                PipeMode::None if matches!(self.unifier.resolve_ty(ty), Ty::Unit) => {
                    self.unify_ty(ty, &Ty::Unit, why.clone());
                }
                PipeMode::None => {
                    let expected = CompTy::Return(
                        PipeSpec {
                            input: spec.input,
                            output: spec.output,
                            result: PipeMode::Bytes,
                        },
                        Box::new(Ty::Unit),
                    );
                    let actual = CompTy::Return(*spec, Box::new(ty.clone()));
                    self.unify_comp_ty(&expected, &actual, why.clone());
                }
            }
        }
        (PipeMode::Bytes, Ty::Unit)
    }

    /// The value side of an arm join: open results pin `∅` and the values
    /// unify into one payload type.
    fn conclude_value_side(&mut self, arms: &[(PipeSpec, Ty)], why: &Reason) -> (PipeMode, Ty) {
        for (spec, _) in arms {
            if matches!(self.unifier.resolve_mode(&spec.result), PipeMode::Var(_)) {
                self.unify_mode(&spec.result, &PipeMode::None, Reason::ResultPin);
            }
        }
        let mut values = arms.iter().map(|(_, ty)| ty.clone());
        let first = values.next().expect("a join always has at least one arm");
        for ty in values {
            self.unify_ty(&first, &ty, why.clone());
        }
        (PipeMode::None, first)
    }

    // ── the worklist: retry stored constraints, then collapse the residue ──

    /// Retry until a full pass retires nothing — a conclusion may determine
    /// a sibling constraint.  Terminates because the lattice has height one
    /// and each conclusion drops its own constraint.
    fn retry_to_quiescence(&mut self) {
        loop {
            let pending = std::mem::take(&mut self.mode_constraints);
            let before = pending.len();
            let kept: Vec<ModeConstraint> =
                pending.into_iter().filter_map(|c| self.retry(c)).collect();
            debug_assert!(
                self.mode_constraints.is_empty(),
                "no conclusion rule emits constraints; a push here would be lost"
            );
            self.mode_constraints = kept;
            if self.mode_constraints.len() == before {
                return;
            }
        }
    }

    /// One quiescence pass over a single constraint: `Some` keeps it stored
    /// unchanged, `None` means it concluded and applied its side effects.
    fn retry(&mut self, c: ModeConstraint) -> Option<ModeConstraint> {
        let saved = std::mem::replace(&mut self.pos, c.pos());
        let out = match c {
            ModeConstraint::Join {
                target,
                ends,
                pos,
                why,
            } => match self.conclude_join(&ends) {
                Some(mode) => {
                    self.unify_mode(&target, &mode, why);
                    None
                }
                None => Some(ModeConstraint::Join {
                    target,
                    ends,
                    pos,
                    why,
                }),
            },
            ModeConstraint::Alt {
                target,
                ends,
                pos,
                why,
            } => match self.conclude_alt(&ends) {
                Some(AltVerdict::Agree(mode)) => {
                    self.unify_mode(&target, &mode, why);
                    None
                }
                Some(AltVerdict::Disagree) => None,
                None => Some(ModeConstraint::Alt {
                    target,
                    ends,
                    pos,
                    why,
                }),
            },
            ModeConstraint::ArmResults {
                result,
                value,
                arms,
                pos,
                why,
            } => match self.conclude_arm_results(&arms, &why) {
                Some((mode, ty)) => {
                    self.unify_mode(&result, &mode, why.clone());
                    self.unify_ty(&value, &ty, why);
                    None
                }
                None => Some(ModeConstraint::ArmResults {
                    result,
                    value,
                    arms,
                    pos,
                    why,
                }),
            },
        };
        self.pos = saved;
        out
    }

    /// Collapse a residue whose verdict settled state directs: a `Join`/`Alt`
    /// target a neighbour grounded from outside — the join itself never
    /// grounds its target without concluding — or an `ArmResults`, whichever
    /// side it lands on.  Writes grounds; the caller re-runs the worklist
    /// after each.
    fn collapse_ground(&mut self, c: ModeConstraint) {
        let saved = std::mem::replace(&mut self.pos, c.pos());
        match c {
            ModeConstraint::Join {
                target, ends, why, ..
            } => match self.unifier.resolve_mode(&target) {
                // `Bytes` satisfies the residue outright: a form's end may
                // exceed its parts' use — a claimed channel nobody writes
                // reads as EOF — and the join never writes back into an end.
                PipeMode::Bytes => {}
                // `∅` is a bound the parts must live within: an end that
                // might still use the channel is told there is none.  A
                // ground `Bytes` end cannot occur here — retry would have
                // concluded the join.
                PipeMode::None => self.pin_open_ends(&ends, &why),
                PipeMode::Var(_) => unreachable!("writes_ground saw this target ground"),
            },
            ModeConstraint::Alt {
                target, ends, why, ..
            } => match self.unifier.resolve_mode(&target) {
                // Provided in excess: arms that read are fed, arms that
                // don't are not disciplined into reading.
                PipeMode::Bytes => {}
                // Nothing provided: an arm that might still read is told the
                // channel is absent; a ground `Bytes` arm beside it keeps
                // the alternation's leniency, as a ground disagreement would.
                PipeMode::None => self.pin_open_ends(&ends, &why),
                // Open target, some ground end: agree where possible — fold
                // the ends together, first failure leaving the target free.
                PipeMode::Var(_) => self.equate_alt(target, ends, why),
            },
            ModeConstraint::ArmResults {
                result,
                value,
                arms,
                why,
                ..
            } => match self.unifier.resolve_mode(&result) {
                // The join's own target grounded from outside — a downstream
                // consumer pinned the form's result — so the arms land on
                // that side with the full protocol, not a bare equation.
                PipeMode::Bytes => {
                    let (_, ty) = self.conclude_byte_side(&arms, &why);
                    self.unify_ty(&value, &ty, why);
                }
                PipeMode::None => {
                    let (_, ty) = self.conclude_value_side(&arms, &why);
                    self.unify_ty(&value, &ty, why);
                }
                PipeMode::Var(_) => {
                    // A ground-`∅` arm whose value never became `Unit` has,
                    // by now, spent every chance to subsume onto a byte
                    // side: it decides the value side, pinning the arms
                    // still open.  Otherwise equate, so the whole join stays
                    // one variable a later grounding can still move.
                    let payload = arms.iter().any(|(spec, ty)| {
                        matches!(self.unifier.resolve_mode(&spec.result), PipeMode::None)
                            && !matches!(self.unifier.resolve_ty(ty), Ty::Unit)
                    });
                    if payload {
                        let (mode, ty) = self.conclude_value_side(&arms, &why);
                        self.unify_mode(&result, &mode, why.clone());
                        self.unify_ty(&value, &ty, why.clone());
                    } else {
                        let open: Vec<PipeMode> = arms
                            .iter()
                            .map(|(spec, _)| spec.result)
                            .filter(|m| matches!(self.unifier.resolve_mode(m), PipeMode::Var(_)))
                            .collect();
                        let mut open = open.into_iter();
                        if let Some(first) = open.next() {
                            for other in open {
                                self.unify_mode(&first, &other, why.clone());
                            }
                            self.unify_mode(&result, &first, why.clone());
                        }
                        for (_, ty) in &arms {
                            self.unify_ty(ty, &value, why.clone());
                        }
                    }
                }
            },
        }
        self.pos = saved;
    }

    fn pin_open_ends(&mut self, ends: &[PipeMode], why: &Reason) {
        for end in ends {
            if matches!(self.unifier.resolve_mode(end), PipeMode::Var(_)) {
                self.unify_mode(end, &PipeMode::None, why.clone());
            }
        }
    }

    /// The all-open residue at generalisation: equate, never default — a
    /// collapsed variable can still ground `Bytes` later, and the target
    /// rides it.  Pure union-find merging, so the order these run in cannot
    /// be observed.
    fn collapse_open(&mut self, c: ModeConstraint) {
        let saved = std::mem::replace(&mut self.pos, c.pos());
        match c {
            ModeConstraint::Join {
                target, ends, why, ..
            } => {
                let open: Vec<PipeMode> = ends
                    .into_iter()
                    .filter(|m| matches!(self.unifier.resolve_mode(m), PipeMode::Var(_)))
                    .collect();
                let mut open = open.into_iter();
                if let Some(first) = open.next() {
                    for other in open {
                        self.unify_mode(&first, &other, why.clone());
                    }
                    self.unify_mode(&target, &first, why);
                }
            }
            ModeConstraint::Alt {
                target, ends, why, ..
            } => self.equate_alt(target, ends, why),
            ModeConstraint::ArmResults { .. } => {
                unreachable!("every ArmResults writes ground state and collapses in the loop")
            }
        }
        self.pos = saved;
    }

    /// Pairwise agreement, n-ary and late: unify every end against the
    /// first, in order; the first failure stops the fold and leaves the
    /// target free.
    fn equate_alt(&mut self, target: PipeMode, ends: Vec<PipeMode>, why: Reason) {
        let mut ends = ends.into_iter();
        if let Some(first) = ends.next() {
            let mut agree = true;
            for next in ends {
                if self.unifier.unify_mode(&first, &next).is_err() {
                    agree = false;
                    break;
                }
            }
            if agree {
                self.unify_mode(&target, &first, why);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typecheck::Scheme;
    use crate::typecheck::ty::ModeVar;

    fn is_open(m: PipeMode) -> bool {
        matches!(m, PipeMode::Var(_))
    }

    /// An arm spec carrying only a payload conduit: chatter is independent of
    /// where the payload rides, so these joins never consult `output`.
    fn spec(result: PipeMode) -> PipeSpec {
        PipeSpec {
            input: PipeMode::None,
            output: PipeMode::None,
            result,
        }
    }

    // ── Join ──

    #[test]
    fn join_any_bytes_dominates() {
        let mut ctx = InferCtx::new();
        let t = ctx.unifier.fresh_mode();
        let mode = ctx.join_modes(vec![t, PipeMode::Bytes], Reason::SeqChannels);
        assert_eq!(mode, PipeMode::Bytes);
        assert!(ctx.mode_constraints.is_empty());
    }

    #[test]
    fn join_all_none_is_none() {
        let mut ctx = InferCtx::new();
        let mode = ctx.join_modes(vec![PipeMode::None, PipeMode::None], Reason::SeqChannels);
        assert_eq!(mode, PipeMode::None);
        assert!(ctx.mode_constraints.is_empty());
    }

    #[test]
    fn join_empty_is_none() {
        let mut ctx = InferCtx::new();
        let mode = ctx.join_modes(vec![], Reason::SeqChannels);
        assert_eq!(mode, PipeMode::None);
    }

    /// `∅ ⊔ μ = μ`: the one open end *is* the join, unmodified — not
    /// defaulted, not a fresh copy.
    #[test]
    fn join_identity_law_returns_the_open_end_itself() {
        let mut ctx = InferCtx::new();
        let t = ctx.unifier.fresh_mode();
        let mode = ctx.join_modes(vec![PipeMode::None, t], Reason::SeqChannels);
        let PipeMode::Var(ModeVar(mode_id)) = ctx.unifier.resolve_mode(&mode) else {
            panic!("identity law must stay a variable, not ground");
        };
        let PipeMode::Var(ModeVar(t_id)) = ctx.unifier.resolve_mode(&t) else {
            panic!("t must stay open");
        };
        assert_eq!(mode_id, t_id, "the join must be t's own variable");
        assert!(ctx.mode_constraints.is_empty());
    }

    #[test]
    fn join_two_open_ends_defer() {
        let mut ctx = InferCtx::new();
        let a = ctx.unifier.fresh_mode();
        let b = ctx.unifier.fresh_mode();
        let target = ctx.join_modes(vec![a, b], Reason::SeqChannels);
        assert_eq!(ctx.mode_constraints.len(), 1);
        assert!(is_open(ctx.unifier.resolve_mode(&target)));
    }

    // ── Alt ──

    #[test]
    fn alt_all_ground_equal_agrees() {
        let mut ctx = InferCtx::new();
        let mode = ctx.alt_modes(vec![PipeMode::Bytes, PipeMode::Bytes], Reason::ScopeArms);
        assert_eq!(mode, PipeMode::Bytes);
        assert!(ctx.mode_constraints.is_empty());
    }

    #[test]
    fn alt_all_ground_disagree_frees_the_target() {
        let mut ctx = InferCtx::new();
        let mode = ctx.alt_modes(vec![PipeMode::Bytes, PipeMode::None], Reason::ScopeArms);
        assert!(is_open(ctx.unifier.resolve_mode(&mode)));
        assert!(ctx.mode_constraints.is_empty());
    }

    #[test]
    fn alt_open_end_defers() {
        let mut ctx = InferCtx::new();
        let a = ctx.unifier.fresh_mode();
        let target = ctx.alt_modes(vec![a, PipeMode::Bytes], Reason::ScopeArms);
        assert_eq!(ctx.mode_constraints.len(), 1);
        assert!(is_open(ctx.unifier.resolve_mode(&target)));
    }

    #[test]
    fn alt_empty_is_a_fresh_variable() {
        let mut ctx = InferCtx::new();
        let mode = ctx.alt_modes(vec![], Reason::ScopeArms);
        assert!(is_open(ctx.unifier.resolve_mode(&mode)));
        assert!(ctx.mode_constraints.is_empty());
    }

    // ── ArmResults ──

    #[test]
    fn arm_results_empty_is_none_and_fresh_value() {
        let mut ctx = InferCtx::new();
        let (result, value) = ctx.join_arm_results(vec![], Reason::CaseArms);
        assert_eq!(result, PipeMode::None);
        assert!(matches!(ctx.unifier.resolve_ty(&value), Ty::Var(_)));
    }

    #[test]
    fn arm_results_byte_side_pins_opens_and_ties_values_to_unit() {
        let mut ctx = InferCtx::new();
        let open_result = ctx.unifier.fresh_mode();
        let open_value = ctx.unifier.fresh_ty();
        let arms = vec![
            (spec(PipeMode::Bytes), Ty::Unit),
            (spec(open_result), open_value.clone()),
        ];
        let (result, value) = ctx.join_arm_results(arms, Reason::CaseArms);
        assert_eq!(result, PipeMode::Bytes);
        assert_eq!(value, Ty::Unit);
        assert_eq!(ctx.unifier.resolve_mode(&open_result), PipeMode::Bytes);
        assert_eq!(ctx.unifier.resolve_ty(&open_value), Ty::Unit);
        assert!(ctx.mode_constraints.is_empty());
        assert!(ctx.errors.is_empty());
    }

    #[test]
    fn arm_results_byte_side_reports_ground_none_nonunit_as_return_mismatch() {
        let mut ctx = InferCtx::new();
        let arms = vec![
            (spec(PipeMode::Bytes), Ty::Unit),
            (spec(PipeMode::None), Ty::Int),
        ];
        let (result, value) = ctx.join_arm_results(arms, Reason::CaseArms);
        assert_eq!(result, PipeMode::Bytes);
        assert_eq!(value, Ty::Unit);
        assert_eq!(
            ctx.errors.len(),
            1,
            "the ∅-at-Int arm must report exactly once"
        );
    }

    /// A ground `∅`-at-non-`Unit` arm beside a still-open arm decides
    /// nothing yet — the open arm may ground `Bytes`, and the conduit
    /// verdict must wait for it — so the join defers, and only the boundary
    /// pins the value side.
    #[test]
    fn arm_results_ground_payload_beside_open_arm_defers_then_pins_value_side() {
        let mut ctx = InferCtx::new();
        let open_result = ctx.unifier.fresh_mode();
        let open_value = ctx.unifier.fresh_ty();
        let arms = vec![
            (spec(PipeMode::None), Ty::Int),
            (spec(open_result), open_value.clone()),
        ];
        let (result, value) = ctx.join_arm_results(arms, Reason::CaseArms);
        assert_eq!(ctx.mode_constraints.len(), 1, "the open arm must defer");
        assert!(is_open(ctx.unifier.resolve_mode(&result)));

        ctx.solve_and_finalize();

        assert_eq!(ctx.unifier.resolve_mode(&result), PipeMode::None);
        assert_eq!(ctx.unifier.resolve_ty(&value), Ty::Int);
        assert_eq!(ctx.unifier.resolve_mode(&open_result), PipeMode::None);
        assert_eq!(ctx.unifier.resolve_ty(&open_value), Ty::Int);
        assert!(ctx.errors.is_empty());
    }

    /// The same start, but the open arm grounds `Bytes` before the boundary:
    /// the join lands on the byte side and the ground `∅`-at-`Int` arm is the
    /// conduit mismatch, reported under the join's own reason — not an early
    /// value-side pin surfacing wherever the `Bytes` grounding happens.
    #[test]
    fn arm_results_ground_payload_beside_late_byte_arm_is_the_joins_own_mismatch() {
        let mut ctx = InferCtx::new();
        let open_result = ctx.unifier.fresh_mode();
        let arms = vec![
            (spec(PipeMode::None), Ty::Int),
            (spec(open_result), Ty::Unit),
        ];
        let (result, value) = ctx.join_arm_results(arms, Reason::CaseArms);
        assert_eq!(ctx.mode_constraints.len(), 1, "the open arm must defer");

        ctx.unify_mode(&open_result, &PipeMode::Bytes, Reason::ResultPin);
        assert!(ctx.errors.is_empty(), "the grounding site stays blameless");
        ctx.solve_and_finalize();

        assert_eq!(ctx.unifier.resolve_mode(&result), PipeMode::Bytes);
        assert_eq!(ctx.unifier.resolve_ty(&value), Ty::Unit);
        assert_eq!(ctx.errors.len(), 1, "one conduit mismatch, at the join");
        assert!(matches!(ctx.errors[0].reason, Some(Reason::CaseArms)));
    }

    #[test]
    fn arm_results_undetermined_defers_and_does_not_unify_values_yet() {
        let mut ctx = InferCtx::new();
        let r1 = ctx.unifier.fresh_mode();
        let r2 = ctx.unifier.fresh_mode();
        let v1 = ctx.unifier.fresh_ty();
        let v2 = ctx.unifier.fresh_ty();
        let arms = vec![(spec(r1), v1.clone()), (spec(r2), v2.clone())];
        let (result, value) = ctx.join_arm_results(arms, Reason::CaseArms);
        assert_eq!(ctx.mode_constraints.len(), 1);
        assert!(is_open(ctx.unifier.resolve_mode(&result)));
        assert!(matches!(ctx.unifier.resolve_ty(&value), Ty::Var(_)));

        // The two arm values must still be independent: pinning one to a
        // concrete type must not have already tied the other to anything.
        ctx.unify_ty(&v1, &Ty::Int, Reason::CaseArms);
        assert!(ctx.errors.is_empty());
        assert!(matches!(ctx.unifier.resolve_ty(&v2), Ty::Var(_)));
    }

    // ── deferral buys something ──

    #[test]
    fn deferred_join_reevaluates_once_an_end_grounds_bytes() {
        let mut ctx = InferCtx::new();
        let a = ctx.unifier.fresh_mode();
        let b = ctx.unifier.fresh_mode();
        let target = ctx.join_modes(vec![a, b], Reason::SeqChannels);
        assert_eq!(ctx.mode_constraints.len(), 1, "two open ends must defer");

        ctx.unify_mode(&a, &PipeMode::Bytes, Reason::SeqChannels);
        ctx.solve_and_finalize();

        assert_eq!(ctx.unifier.resolve_mode(&target), PipeMode::Bytes);
        // Join constrains the target only: `b` itself is never touched.
        assert!(is_open(ctx.unifier.resolve_mode(&b)));
    }

    #[test]
    fn deferred_arm_results_reevaluates_on_the_byte_side_and_closes_wf2() {
        let mut ctx = InferCtx::new();
        let r1 = ctx.unifier.fresh_mode();
        let r2 = ctx.unifier.fresh_mode();
        let v1 = ctx.unifier.fresh_ty();
        let v2 = ctx.unifier.fresh_ty();
        let (result, value) = ctx.join_arm_results(
            vec![(spec(r1), v1.clone()), (spec(r2), v2.clone())],
            Reason::CaseArms,
        );
        assert_eq!(ctx.mode_constraints.len(), 1, "two open arms must defer");

        ctx.unify_mode(&r1, &PipeMode::Bytes, Reason::ResultPin);
        ctx.solve_and_finalize();

        assert_eq!(ctx.unifier.resolve_mode(&result), PipeMode::Bytes);
        assert_eq!(ctx.unifier.resolve_ty(&value), Ty::Unit);
        assert_eq!(ctx.unifier.resolve_mode(&r2), PipeMode::Bytes);
        // WF-2 closed: v2's arm was still open when r1 grounded, and its
        // value is tied to Unit all the same.
        assert_eq!(ctx.unifier.resolve_ty(&v1), Ty::Unit);
        assert_eq!(ctx.unifier.resolve_ty(&v2), Ty::Unit);
    }

    // ── collapse ──

    #[test]
    fn collapse_preserves_mode_polymorphism() {
        let mut ctx = InferCtx::new();
        let a = ctx.unifier.fresh_mode();
        let b = ctx.unifier.fresh_mode();
        let target = ctx.join_modes(vec![a, b], Reason::SeqChannels);

        ctx.solve_and_finalize();

        assert!(is_open(ctx.unifier.resolve_mode(&a)));
        assert!(is_open(ctx.unifier.resolve_mode(&b)));
        assert!(is_open(ctx.unifier.resolve_mode(&target)));
        assert_eq!(ctx.unifier.resolve_mode(&a), ctx.unifier.resolve_mode(&b));
        assert_eq!(
            ctx.unifier.resolve_mode(&a),
            ctx.unifier.resolve_mode(&target)
        );

        // Grounding any one of them afterwards grounds the whole equated set.
        ctx.unify_mode(&a, &PipeMode::Bytes, Reason::SeqChannels);
        assert_eq!(ctx.unifier.resolve_mode(&b), PipeMode::Bytes);
        assert_eq!(ctx.unifier.resolve_mode(&target), PipeMode::Bytes);
    }

    #[test]
    fn collapse_equates_alt_ends_still_open() {
        let mut ctx = InferCtx::new();
        let a = ctx.unifier.fresh_mode();
        let b = ctx.unifier.fresh_mode();
        let target = ctx.alt_modes(vec![a, b], Reason::ScopeArms);

        ctx.solve_and_finalize();

        assert!(is_open(ctx.unifier.resolve_mode(&target)));
        assert_eq!(ctx.unifier.resolve_mode(&a), ctx.unifier.resolve_mode(&b));
        assert_eq!(
            ctx.unifier.resolve_mode(&a),
            ctx.unifier.resolve_mode(&target)
        );
    }

    #[test]
    fn collapse_ties_arm_values_even_when_residue_never_picked_a_side() {
        let mut ctx = InferCtx::new();
        // Both arms ∅-at-Unit: the join never determines which side wins,
        // so it rides to collapse untouched.
        let v1 = ctx.unifier.fresh_ty();
        let v2 = ctx.unifier.fresh_ty();
        ctx.unify_ty(&v1, &Ty::Unit, Reason::CaseArms);
        ctx.unify_ty(&v2, &Ty::Unit, Reason::CaseArms);
        let r1 = ctx.unifier.fresh_mode();
        let r2 = ctx.unifier.fresh_mode();
        let (result, value) = ctx.join_arm_results(
            vec![(spec(r1), v1.clone()), (spec(r2), v2.clone())],
            Reason::CaseArms,
        );
        assert_eq!(ctx.mode_constraints.len(), 1);

        ctx.solve_and_finalize();

        assert!(is_open(ctx.unifier.resolve_mode(&result)));
        assert_eq!(ctx.unifier.resolve_mode(&r1), ctx.unifier.resolve_mode(&r2));
        assert_eq!(ctx.unifier.resolve_ty(&value), Ty::Unit);
    }

    // ── order-insensitivity ──

    /// `Join` and `Alt` share the variable `b`. Emitting the two constraints
    /// and grounding `b` to `Bytes` in every order must land on the same
    /// verdict: the join dominates to `Bytes`, the alt's still-open sibling
    /// `c` gets equated and grounded through collapse, and `a` — touched by
    /// no rule, ever — stays open.
    fn join_alt_scenario(order: [u8; 3]) -> (PipeMode, PipeMode, PipeMode, PipeMode) {
        let mut ctx = InferCtx::new();
        let a = ctx.unifier.fresh_mode();
        let b = ctx.unifier.fresh_mode();
        let c = ctx.unifier.fresh_mode();
        let mut join_target = None;
        let mut alt_target = None;
        for step in order {
            match step {
                0 => join_target = Some(ctx.join_modes(vec![a, b], Reason::SeqChannels)),
                1 => alt_target = Some(ctx.alt_modes(vec![b, c], Reason::ScopeArms)),
                2 => ctx.unify_mode(&b, &PipeMode::Bytes, Reason::ScopeArms),
                _ => unreachable!(),
            }
        }
        ctx.solve_and_finalize();
        (
            ctx.unifier.resolve_mode(&a),
            ctx.unifier
                .resolve_mode(&join_target.expect("emitted above")),
            ctx.unifier
                .resolve_mode(&alt_target.expect("emitted above")),
            ctx.unifier.resolve_mode(&c),
        )
    }

    #[test]
    fn order_insensitive_join_and_alt_sharing_a_variable() {
        let permutations: [[u8; 3]; 6] = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        let results = permutations.map(join_alt_scenario);
        for (a, join, alt, c) in results {
            assert!(is_open(a), "a is never a party to any rule");
            assert_eq!(join, PipeMode::Bytes);
            assert_eq!(alt, PipeMode::Bytes);
            assert_eq!(c, PipeMode::Bytes);
        }
    }

    /// An `ArmResults` join over two arms, grounding each arm's result to
    /// `Bytes` independently. Whether the join is emitted before, between,
    /// or after the two groundings, every arm ends up byte-pinned and every
    /// value tied to `Unit`.
    fn arm_results_scenario(order: [u8; 3]) -> (PipeMode, Ty, Ty, Ty) {
        let mut ctx = InferCtx::new();
        let r1 = ctx.unifier.fresh_mode();
        let r2 = ctx.unifier.fresh_mode();
        let v1 = ctx.unifier.fresh_ty();
        let v2 = ctx.unifier.fresh_ty();
        let mut result_target = None;
        let mut value_target = None;
        for step in order {
            match step {
                0 => {
                    let (result, value) = ctx.join_arm_results(
                        vec![(spec(r1), v1.clone()), (spec(r2), v2.clone())],
                        Reason::CaseArms,
                    );
                    result_target = Some(result);
                    value_target = Some(value);
                }
                1 => ctx.unify_mode(&r1, &PipeMode::Bytes, Reason::ResultPin),
                2 => ctx.unify_mode(&r2, &PipeMode::Bytes, Reason::ResultPin),
                _ => unreachable!(),
            }
        }
        ctx.solve_and_finalize();
        (
            ctx.unifier
                .resolve_mode(&result_target.expect("emitted above")),
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
            assert_eq!(result, PipeMode::Bytes);
            assert_eq!(value, Ty::Unit);
            assert_eq!(v1, Ty::Unit);
            assert_eq!(v2, Ty::Unit);
        }
    }

    // ── boundaries own their constraints ──

    /// An environment scheme mentioning every writable position of the
    /// constraint: the constraint belongs to whichever enclosing binding
    /// that scheme stands in for.
    fn env_owning(modes: [PipeMode; 3]) -> TyEnv {
        let [input, output, result] = modes;
        let mut env = TyEnv::new();
        env.bind(
            "owner".into(),
            Scheme::mono(Ty::Thunk(Box::new(CompTy::Return(
                PipeSpec {
                    input,
                    output,
                    result,
                },
                Box::new(Ty::Unit),
            )))),
        );
        env
    }

    /// A boundary must not touch a constraint whose variables all belong to
    /// the environment — neither conclude it (its side effects would pin
    /// arms still under inference elsewhere) nor collapse it.  The owning
    /// drain later lands it on the byte side with subsumption intact.
    #[test]
    fn boundary_leaves_env_owned_constraints_untouched() {
        let mut ctx = InferCtx::new();
        let r1 = ctx.unifier.fresh_mode();
        let r2 = ctx.unifier.fresh_mode();
        let (result, _) = ctx.join_arm_results(
            vec![(spec(r1), Ty::Unit), (spec(r2), Ty::Unit)],
            Reason::CaseArms,
        );
        let env = env_owning([r1, r2, result]);

        ctx.solve_at_boundary(&env);

        assert_eq!(ctx.mode_constraints.len(), 1, "the constraint is kept");
        assert!(is_open(ctx.unifier.resolve_mode(&r1)));
        assert!(is_open(ctx.unifier.resolve_mode(&r2)));
        assert_ne!(
            ctx.unifier.resolve_mode(&r1),
            ctx.unifier.resolve_mode(&r2),
            "sibling arms stay independent past a boundary that does not own them"
        );

        // The arms ground apart only afterwards; the owning drain still
        // applies the byte side with the ∅@Unit subsumption intact.
        ctx.unify_mode(&r1, &PipeMode::Bytes, Reason::ResultPin);
        ctx.solve_and_finalize();
        assert_eq!(ctx.unifier.resolve_mode(&result), PipeMode::Bytes);
        assert_eq!(ctx.unifier.resolve_mode(&r2), PipeMode::Bytes);
        assert!(ctx.errors.is_empty());
    }

    #[test]
    fn boundary_collapses_what_it_owns() {
        let mut ctx = InferCtx::new();
        let a = ctx.unifier.fresh_mode();
        let b = ctx.unifier.fresh_mode();
        let target = ctx.join_modes(vec![a, b], Reason::SeqChannels);

        // `a` is the environment's; `b` and the target are this boundary's,
        // so the constraint is local and collapses to one equated class.
        ctx.solve_at_boundary(&env_owning([a, PipeMode::None, PipeMode::None]));

        assert!(ctx.mode_constraints.is_empty());
        assert_eq!(ctx.unifier.resolve_mode(&a), ctx.unifier.resolve_mode(&b));
        assert_eq!(
            ctx.unifier.resolve_mode(&a),
            ctx.unifier.resolve_mode(&target)
        );
    }

    // ── collapse is directed by a ground target, never through it ──

    /// A target grounded `Bytes` from outside satisfies the join outright:
    /// the form's end may exceed its parts' use, and collapse must not write
    /// the target back into the ends.
    #[test]
    fn ground_bytes_target_drops_the_join_without_touching_ends() {
        let mut ctx = InferCtx::new();
        let a = ctx.unifier.fresh_mode();
        let b = ctx.unifier.fresh_mode();
        let target = ctx.join_modes(vec![a, b], Reason::SeqChannels);

        ctx.unify_mode(&target, &PipeMode::Bytes, Reason::SeqChannels);
        ctx.solve_and_finalize();

        assert!(is_open(ctx.unifier.resolve_mode(&a)));
        assert!(is_open(ctx.unifier.resolve_mode(&b)));
        assert_ne!(
            ctx.unifier.resolve_mode(&a),
            ctx.unifier.resolve_mode(&b),
            "a satisfied join must not equate its ends either"
        );
    }

    #[test]
    fn ground_none_target_pins_open_ends() {
        let mut ctx = InferCtx::new();
        let a = ctx.unifier.fresh_mode();
        let b = ctx.unifier.fresh_mode();
        let target = ctx.join_modes(vec![a, b], Reason::SeqChannels);

        ctx.unify_mode(&target, &PipeMode::None, Reason::SeqChannels);
        ctx.solve_and_finalize();

        assert_eq!(ctx.unifier.resolve_mode(&a), PipeMode::None);
        assert_eq!(ctx.unifier.resolve_mode(&b), PipeMode::None);
        assert!(ctx.errors.is_empty());
    }

    /// An `ArmResults` whose result a downstream consumer grounded `Bytes`
    /// lands on the byte side with the full protocol — values tied to `Unit`
    /// — not a bare equation of the open results.
    #[test]
    fn ground_bytes_result_directs_arm_results_onto_the_byte_side() {
        let mut ctx = InferCtx::new();
        let r1 = ctx.unifier.fresh_mode();
        let r2 = ctx.unifier.fresh_mode();
        let v1 = ctx.unifier.fresh_ty();
        let v2 = ctx.unifier.fresh_ty();
        let (result, value) = ctx.join_arm_results(
            vec![(spec(r1), v1.clone()), (spec(r2), v2.clone())],
            Reason::CaseArms,
        );

        ctx.unify_mode(&result, &PipeMode::Bytes, Reason::ResultPin);
        ctx.solve_and_finalize();

        assert_eq!(ctx.unifier.resolve_mode(&r1), PipeMode::Bytes);
        assert_eq!(ctx.unifier.resolve_mode(&r2), PipeMode::Bytes);
        assert_eq!(ctx.unifier.resolve_ty(&v1), Ty::Unit);
        assert_eq!(ctx.unifier.resolve_ty(&v2), Ty::Unit);
        assert_eq!(ctx.unifier.resolve_ty(&value), Ty::Unit);
    }

    /// Two joins share the end `b`; one join's target is grounded `Bytes`
    /// from outside.  Whichever order the two are emitted, the satisfied
    /// join drops and the other equates — `b` must come out the same.
    fn shared_end_scenario(swap: bool) -> (PipeMode, PipeMode, PipeMode) {
        let mut ctx = InferCtx::new();
        let a = ctx.unifier.fresh_mode();
        let b = ctx.unifier.fresh_mode();
        let c = ctx.unifier.fresh_mode();
        let (t1, t2) = if swap {
            let t2 = ctx.join_modes(vec![b, c], Reason::SeqChannels);
            let t1 = ctx.join_modes(vec![a, b], Reason::SeqChannels);
            (t1, t2)
        } else {
            let t1 = ctx.join_modes(vec![a, b], Reason::SeqChannels);
            let t2 = ctx.join_modes(vec![b, c], Reason::SeqChannels);
            (t1, t2)
        };
        ctx.unify_mode(&t1, &PipeMode::Bytes, Reason::SeqChannels);
        ctx.solve_and_finalize();
        let _ = t2;
        (
            ctx.unifier.resolve_mode(&a),
            ctx.unifier.resolve_mode(&b),
            ctx.unifier.resolve_mode(&c),
        )
    }

    #[test]
    fn order_insensitive_collapse_under_an_externally_ground_target() {
        let (a0, b0, c0) = shared_end_scenario(false);
        let (a1, b1, c1) = shared_end_scenario(true);
        assert!(is_open(a0) && is_open(b0) && is_open(c0));
        assert!(is_open(a1) && is_open(b1) && is_open(c1));
        assert_eq!(b0, c0, "the live join equates its own ends");
        assert_eq!(b1, c1);
        assert_ne!(a0, b0, "the satisfied join keeps out of it");
        assert_ne!(a1, b1);
    }
}
