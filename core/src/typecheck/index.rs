//! Reads `$c[k]` whose target's head is not yet known, deferred until it is.
//!
//! `Lbl(c, ℓ, e)` is `c = [ℓ: e | ρ] ∨ c = Map e`.  A head settles it, and so
//! does a kind that admits one of the two and not the other; a target still
//! undecided waits, and is read as a record at the boundary that owns it (or
//! at the unit's end), never refused for want of information.
//!
//! `Idx(c, k, e)` is `c = [e] ∧ k = Int ∨ c = Map e ∧ k = String`, for a key
//! that is not a literal.  Its three variables are weak for the whole unit, so
//! nothing generalises over it and settling it cannot change which variables a
//! `let` quantifies.  Nothing is settled at a generalisation; what is still
//! pending when the unit ends is refused.

use super::env::{InferCtx, TyEnv};
use super::error::{KindFound, Reason, TypeErrorKind};
use super::generalize::{FreeVars, env_free_vars, free_ty};
use super::kind::{Head, Kind};
use super::ty::{Label, Row, Ty, TyVar};
use crate::source::Span;

pub(super) struct Lbl {
    pub target: Ty,
    pub label: String,
    pub elem: Ty,
    pub span: Option<Span>,
}

/// A computed read: `target[key]` of type `elem`.
pub(super) struct Idx {
    pub target: Ty,
    pub key: Ty,
    pub elem: Ty,
    pub span: Option<Span>,
    /// The target and key as written, when both are variables.
    pub names: Option<(String, String)>,
    /// The binding whose value holds the read.
    pub holder: Option<Span>,
}

pub(super) enum Settled {
    Done,
    Pending,
}

/// What an index's container is, once decided.
#[derive(Clone, Copy)]
enum Shape {
    List,
    Map,
}

/// One pass over `pending`: what stays pending, and whether anything settled.
fn retry<T>(pending: Vec<T>, mut settle: impl FnMut(&T) -> Settled) -> (Vec<T>, bool) {
    let before = pending.len();
    let kept: Vec<T> = pending
        .into_iter()
        .filter(|item| matches!(settle(item), Settled::Pending))
        .collect();
    let progress = kept.len() < before;
    (kept, progress)
}

impl InferCtx {
    /// Fire the disjunct the target's head, or else its kind, selects; a
    /// refusal is diagnosed at the read and is `Done`.
    pub(super) fn settle_label(&mut self, lbl: &Lbl) -> Settled {
        let target = self.unifier.apply_ty(&lbl.target);
        let saved = std::mem::replace(&mut self.pos, lbl.span);
        let settled = match target {
            Ty::Var(v) => self.settle_by_kind(lbl, v),
            Ty::Record(_) => {
                self.read_as_record(lbl);
                Settled::Done
            }
            Ty::Map(_) => {
                self.read_as_map(lbl);
                Settled::Done
            }
            Ty::Thunk(_) => {
                self.diagnose(TypeErrorKind::IndexIntoThunk);
                Settled::Done
            }
            ty => {
                self.diagnose(TypeErrorKind::FieldOnNonRecord {
                    label: lbl.label.clone(),
                    ty,
                });
                Settled::Done
            }
        };
        self.pos = saved;
        settled
    }

    /// A variable target is decided by what its kind admits: a read needs a
    /// record or a map, and a kind with neither is refused here, naming what
    /// else the variable was used as.
    fn settle_by_kind(&mut self, lbl: &Lbl, target: TyVar) -> Settled {
        let used = self.unifier.kinded(target);
        match (used.kind.admits(Head::Record), used.kind.admits(Head::Map)) {
            (true, true) => return Settled::Pending,
            (true, false) => self.read_as_record(lbl),
            (false, true) => self.read_as_map(lbl),
            (false, false) => self.diagnose(TypeErrorKind::KindMismatch {
                found: KindFound::Used {
                    kind: used.kind,
                    witness: used.witness,
                },
                kind: Kind::LABELLED,
                witness: lbl.span,
            }),
        }
        Settled::Done
    }

    fn read_as_map(&mut self, lbl: &Lbl) {
        let map = Ty::Map(Box::new(lbl.elem.clone()));
        self.unify_ty(&lbl.target, &map, Reason::MapKeyRead);
    }

    fn read_as_record(&mut self, lbl: &Lbl) {
        let saved = std::mem::replace(&mut self.pos, lbl.span);
        let tail = self.unifier.fresh_row();
        let record = Ty::Record(Row::Extend(
            Label::Field(lbl.label.clone()),
            Box::new(lbl.elem.clone()),
            Box::new(tail),
        ));
        self.unify_ty(&lbl.target, &record, Reason::RecordFieldRead);
        self.pos = saved;
    }

    /// Read the labels as records, all those on one target at once: closing
    /// them one by one would extend the target's row while an earlier read's
    /// element already reaches it, which the row occurs check refuses — `cell`
    /// in `{ |cell| $cell[head]; drain $cell[tail] }`, a stream's tail.
    fn close_as_records(&mut self, pending: Vec<Lbl>) {
        let mut groups: Vec<(Ty, Vec<Lbl>)> = Vec::new();
        for lbl in pending {
            let head = self.unifier.apply_ty(&lbl.target);
            match groups.iter_mut().find(|(g, _)| *g == head) {
                Some((_, group)) => group.push(lbl),
                None => groups.push((head, vec![lbl])),
            }
        }
        for (head, group) in groups {
            if !matches!(self.unifier.apply_ty(&head), Ty::Var(_)) {
                for lbl in &group {
                    self.settle_label(lbl);
                }
                continue;
            }
            let saved = std::mem::replace(&mut self.pos, group[0].span);
            let mut fields: Vec<(&str, &Ty)> = Vec::new();
            for lbl in &group {
                match fields.iter().find(|(label, _)| *label == lbl.label) {
                    Some((_, first)) => {
                        self.pos = lbl.span;
                        self.unify_ty(&lbl.elem, first, Reason::RecordFieldRead);
                    }
                    None => fields.push((&lbl.label, &lbl.elem)),
                }
            }
            self.pos = group[0].span;
            let tail = self.unifier.fresh_row();
            let record = fields.into_iter().rev().fold(tail, |row, (label, elem)| {
                Row::Extend(
                    Label::Field(label.to_string()),
                    Box::new(elem.clone()),
                    Box::new(row),
                )
            });
            self.unify_ty(&head, &Ty::Record(record), Reason::RecordFieldRead);
            self.pos = saved;
        }
    }

    fn retry_labels(&mut self) -> bool {
        let pending = std::mem::take(&mut self.pending_labels);
        let (kept, progress) = retry(pending, |lbl| self.settle_label(lbl));
        self.pending_labels = kept;
        progress
    }

    /// Settle what the store now decides, then read as records the labels
    /// `env` does not own. A label is tied to `env` when its target is free
    /// in `env` or weak, or is the element of a tied label; those are kept, and the
    /// free variables of their elements come back for `generalize` to leave
    /// unquantified. With no `env`, everything still pending is closed.
    pub(super) fn settle_pending_labels(&mut self, env: Option<&TyEnv>) -> FreeVars {
        while self.retry_labels() {}
        let mut tied = FreeVars::new();
        let mut pending = std::mem::take(&mut self.pending_labels);
        if let Some(env) = env {
            let mut reach = env_free_vars(&self.unifier, env);
            let mut kept = Vec::new();
            loop {
                let (hit, rest): (Vec<_>, Vec<_>) = pending.into_iter().partition(|lbl| {
                    matches!(self.unifier.apply_ty(&lbl.target), Ty::Var(v)
                        if reach.tys.contains(&v) || self.unifier.is_weak_ty(v))
                });
                pending = rest;
                if hit.is_empty() {
                    break;
                }
                for lbl in &hit {
                    free_ty(&self.unifier, &lbl.elem, &mut reach);
                }
                kept.extend(hit);
            }
            for lbl in &kept {
                free_ty(&self.unifier, &lbl.elem, &mut tied);
            }
            self.pending_labels = kept;
        }
        self.close_as_records(pending);
        tied
    }
}

impl InferCtx {
    /// Fire the disjunct the target's head, or else the key's or a kind's,
    /// selects.  A literal `Int` key is the list rule and never waits.  A
    /// refusal is diagnosed at the read and is `Done`.
    pub(super) fn settle_index(&mut self, idx: &Idx) -> Settled {
        let saved = std::mem::replace(&mut self.pos, idx.span);
        let settled = match self.unifier.apply_ty(&idx.target) {
            Ty::List(_) => self.read_as(idx, Shape::List),
            Ty::Map(_) => self.read_as(idx, Shape::Map),
            Ty::Var(v) => self.settle_variable(idx, v),
            Ty::Thunk(_) => {
                self.diagnose(TypeErrorKind::IndexIntoThunk);
                Settled::Done
            }
            ty => {
                self.diagnose(TypeErrorKind::DynamicIndexOnScalar { ty });
                Settled::Done
            }
        };
        self.pos = saved;
        settled
    }

    fn read_as(&mut self, idx: &Idx, shape: Shape) -> Settled {
        let (container, key, reason) = match shape {
            Shape::List => (
                Ty::List(Box::new(idx.elem.clone())),
                Ty::Int,
                Reason::ListIndexKey,
            ),
            Shape::Map => (
                Ty::Map(Box::new(idx.elem.clone())),
                Ty::String,
                Reason::MapIndexKey,
            ),
        };
        self.unify_ty(&idx.target, &container, Reason::DynamicIndexTarget);
        self.unify_ty(&idx.key, &key, reason);
        Settled::Done
    }

    /// A variable target takes the shape the key decides, else the shape its
    /// own kind decides, and waits while both admit either.
    fn settle_variable(&mut self, idx: &Idx, target: TyVar) -> Settled {
        let held = self.unifier.kinded(target);
        let by_target = match (held.kind.admits(Head::List), held.kind.admits(Head::Map)) {
            (true, true) => None,
            (true, false) => Some(Shape::List),
            (false, true) => Some(Shape::Map),
            (false, false) => {
                self.refuse_kind(idx, held.kind, held.witness, Kind::INDEXED);
                return Settled::Done;
            }
        };
        let by_key = match self.unifier.apply_ty(&idx.key) {
            Ty::Int => Some(Shape::List),
            Ty::String => Some(Shape::Map),
            Ty::Var(k) => {
                let used = self.unifier.kinded(k);
                match (used.kind.admits(Head::Int), used.kind.admits(Head::String)) {
                    (true, true) => None,
                    (true, false) => Some(Shape::List),
                    (false, true) => Some(Shape::Map),
                    (false, false) => {
                        self.refuse_kind(idx, used.kind, used.witness, Kind::KEY);
                        return Settled::Done;
                    }
                }
            }
            ty => {
                self.diagnose(TypeErrorKind::KindMismatch {
                    found: KindFound::Type(Box::new(ty)),
                    kind: Kind::KEY,
                    witness: idx.span,
                });
                return Settled::Done;
            }
        };
        match by_key.or(by_target) {
            Some(shape) => self.read_as(idx, shape),
            None => Settled::Pending,
        }
    }

    fn refuse_kind(&mut self, idx: &Idx, used: Kind, earlier: Option<Span>, required: Kind) {
        self.diagnose(TypeErrorKind::KindMismatch {
            found: KindFound::Used {
                kind: used,
                witness: earlier,
            },
            kind: required,
            witness: idx.span,
        });
    }

    fn retry_indexes(&mut self) -> bool {
        let pending = std::mem::take(&mut self.pending_indexes);
        let (kept, progress) = retry(pending, |idx| self.settle_index(idx));
        self.pending_indexes = kept;
        progress
    }

    /// The unit's end.  Labels and indexes settle together to a fixpoint,
    /// since either can decide the other's target; an index still pending
    /// is then refused.  Labels still pending are left to
    /// [`Self::settle_pending_labels`], which reads them as records.
    pub(super) fn settle_pending_indexes(&mut self) {
        while self.retry_labels() | self.retry_indexes() {}
        for idx in std::mem::take(&mut self.pending_indexes) {
            let saved = std::mem::replace(&mut self.pos, idx.span);
            self.diagnose(TypeErrorKind::IndexContainerUnknown {
                names: idx.names,
                holder: idx.holder,
            });
            self.pos = saved;
        }
    }
}
