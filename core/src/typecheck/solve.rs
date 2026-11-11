//! Constraints while a unit is open; observations only after it closes.

use super::SessionSchemes;
use super::builtins::Decl;
use super::contract::{self, Key, NestedCheck, Table};
use super::env::{HandlerOrigin, TyEnv};
use super::error::{Clash, Reason, TypeError, TypeErrorKind, UnitCall};
use super::generalize::{generalize, instantiate, reseed_weak, settle_weak};
use super::unify::{Unifier, WeakSource};
use crate::source::{Span, WithSpan};
use crate::ty::{
    Closed, CompTy, CompTyNode, Grade, Kind, RefusedArg, Row, Scheme, Site, Term, Ty, TyNode,
    UnitId, Var,
};
use std::collections::HashMap;
use std::sync::Arc;

/// Owned because an argv obligation outlives constraint generation.
pub(super) enum ArgvBoundary {
    InShell(String),
    Exec(String),
}

/// How a binding receives its right-hand side: runs it for its value, keeps
/// a function as a block, or captures a command's output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Run,
    Thunk,
    Capture,
}

/// A binding whose [`Mode`] the solution decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BindHole(u32);

/// A call whose splits the solution decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CallHole(u32);

pub(super) struct Solver {
    unifier: Unifier,
    errors: Vec<TypeError>,
    /// Suspended matches, under the root of the variable each one scrutinises.
    waiting: HashMap<Var, Vec<Suspended>>,
    modes: Vec<Option<Mode>>,
    /// Per call: the arguments given so far, and the splits performed.
    calls: Vec<(usize, Vec<usize>)>,
    gates: Vec<(Option<Span>, Ty, ArgvBoundary)>,
    unit_reads: Vec<(Option<Span>, Ty, UnitCall)>,
    nested: Vec<NestedCheck>,
}

enum Match {
    Binding {
        hole: BindHole,
        rhs: CompTy,
        binder: Ty,
        forced: Option<String>,
    },
    Argument {
        call: CallHole,
        index: usize,
        head: CompTy,
        callee: Option<String>,
        arg: Ty,
        result: CompTy,
        recursive_call: bool,
    },
}

struct Suspended {
    constraint: Match,
    pos: Option<Span>,
}

impl Suspended {
    fn lower(&self, unifier: &mut Unifier, scrutinee: Var) {
        let terms: &[_] = match &self.constraint {
            Match::Binding { rhs, binder, .. } => &[Term::Comp(rhs), Term::Ty(binder)],
            Match::Argument {
                head, arg, result, ..
            } => &[Term::Comp(head), Term::Ty(arg), Term::Comp(result)],
        };
        for &term in terms {
            unifier.lower_to_scrutinee(term, scrutinee);
        }
    }
}

enum Equation {
    Value(Ty, Ty),
    Computation(CompTy, CompTy),
}

/// A closed unit with every hole decided.
pub(super) struct Solution {
    unifier: Unifier,
    modes: Box<[Mode]>,
    splits: Box<[Box<[usize]>]>,
}

pub(super) fn sort_errors(errors: &mut [TypeError]) {
    errors.sort_by_key(|error| error.pos.map(|span| (span.file, span.start, span.end)));
}

impl WithSpan for Solver {
    fn span_slot(&mut self) -> &mut Option<Span> {
        &mut self.unifier.at
    }
}

impl Solver {
    pub(super) fn new() -> Self {
        Self {
            unifier: Unifier::new(),
            errors: Vec::new(),
            waiting: HashMap::new(),
            modes: Vec::new(),
            calls: Vec::new(),
            gates: Vec::new(),
            unit_reads: Vec::new(),
            nested: Vec::new(),
        }
    }

    pub(super) fn fresh_ty(&mut self) -> Ty {
        self.unifier.fresh_ty()
    }

    pub(super) fn fresh_comp(&mut self) -> CompTy {
        self.unifier.fresh_comp_ty()
    }

    pub(super) fn fresh_row(&mut self) -> Row {
        self.unifier.fresh_row()
    }

    pub(super) fn fresh_grade(&mut self) -> Grade {
        self.unifier.fresh_grade()
    }

    pub(super) fn fresh_kinded(&mut self, kind: Kind) -> Ty {
        self.unifier.fresh_kinded(kind)
    }

    pub(super) fn equate(&mut self, a: &Ty, b: &Ty, why: Reason) -> bool {
        self.constrain(|this| this.unify(a, b, why))
    }

    pub(super) fn equate_comp(&mut self, a: &CompTy, b: &CompTy, why: Reason) -> bool {
        self.constrain(|this| this.unify_comp(a, b, why))
    }

    pub(super) fn equate_row(&mut self, a: &Row, b: &Row, why: Reason) -> bool {
        self.constrain(|this| {
            let outcome = this.unifier.unify_row(a, b);
            this.held(outcome.map_err(|kind| Box::new((kind, why))), |u| {
                let weak = |row: &Row| u.weak_source_in_ty(&Ty::record(row.clone()));
                weak(a).or_else(|| weak(b))
            })
        })
    }

    pub(super) fn equate_held(
        &mut self,
        declared: &Ty,
        found: &Ty,
        table: &'static Table,
        key: &'static Key,
    ) -> bool {
        self.constrain(|this| {
            let outcome = contract::unify_held(table, key)(&mut this.unifier, declared, found);
            this.held(outcome, |u| {
                u.weak_source_in_ty(declared)
                    .or_else(|| u.weak_source_in_ty(found))
            })
        })
    }

    /// Neither instantiation nor contract import exposes its resulting head.
    fn opaque(&mut self, ty: &Ty) -> Ty {
        self.unifier.fresh_alias(ty)
    }

    pub(super) fn instantiate(&mut self, scheme: &Scheme) -> Ty {
        self.constrain(|this| {
            let ty = instantiate(&mut this.unifier, scheme);
            this.opaque(&ty)
        })
    }

    /// A declaration, unlike a scheme token, supplies syntax-known structure.
    pub(super) fn declared(&mut self, decl: &Decl) -> Ty {
        self.constrain(|this| {
            let ty = instantiate(&mut this.unifier, decl.scheme());
            this.unifier.remember_producers(Term::Ty(&ty));
            ty
        })
    }

    pub(super) fn contract(&mut self, site: &Site) -> Ty {
        self.constrain(|this| {
            let ty = site.instantiate(&mut this.unifier);
            this.opaque(&ty)
        })
    }

    /// Seed at rank zero; imported residuals retain one type throughout this unit.
    pub(super) fn seed(&mut self, schemes: SessionSchemes) -> (TyEnv, Vec<(String, Arc<Scheme>)>) {
        assert_eq!(self.unifier.rank(), 0, "environment seeded inside a region");
        self.constrain(|this| {
            let u = &mut this.unifier;
            let mut env = TyEnv::new();
            env.builtins = schemes.builtins;
            let frames: Vec<_> = env
                .builtins
                .base_frames()
                .map(|decl| {
                    let scheme = reseed_weak(u, decl.scheme());
                    (decl.name.as_ref().to_string(), scheme)
                })
                .collect();
            for (name, scheme) in frames {
                env.bind_handler(name, scheme, HandlerOrigin::Within);
            }
            let mut residuals = Vec::new();
            for (name, scheme) in schemes.bindings {
                let scheme = if let Some(scheme) = scheme {
                    reseed_weak(u, &scheme)
                } else {
                    let ty = u.fresh_ty();
                    u.mark_weak(&ty, &WeakSource::Host(name.as_str().into()));
                    Arc::new(Scheme::mono(ty))
                };
                if scheme.has_weak() {
                    residuals.push((name.clone(), Arc::clone(&scheme)));
                }
                env.bind(name, scheme);
            }
            for (name, scheme) in schemes.aliases {
                let scheme = reseed_weak(u, &scheme);
                env.bind_handler(name, scheme, HandlerOrigin::Alias);
            }
            (env, residuals)
        })
    }

    pub(super) fn enter(&mut self) {
        self.unifier.enter();
    }

    pub(super) fn exit(&mut self) {
        self.close_region();
        self.unifier.exit();
    }

    pub(super) fn generalise(&self, ty: &Ty) -> Scheme {
        generalize(&self.unifier, ty)
    }

    /// The binder of a `let` whose right-hand side has type `rhs`, and forces
    /// the variable `forced` when it forces one.
    pub(super) fn binding(&mut self, rhs: &CompTy, forced: Option<&str>) -> (Ty, BindHole) {
        let hole = BindHole(u32::try_from(self.modes.len()).expect("binding holes exhausted"));
        self.modes.push(None);
        let binder = self.fresh_ty();
        self.constrain(|this| {
            this.suspend(Match::Binding {
                hole,
                rhs: rhs.clone(),
                binder: binder.clone(),
                forced: forced.map(str::to_owned),
            });
        });
        (binder, hole)
    }

    pub(super) fn call(&mut self) -> CallHole {
        let hole = CallHole(u32::try_from(self.calls.len()).expect("call holes exhausted"));
        self.calls.push((0, Vec::new()));
        hole
    }

    /// The result of giving `call`'s next argument, `arg`, to `head`, which
    /// the variable `callee` names when it is one.
    pub(super) fn argument(
        &mut self,
        call: CallHole,
        head: &CompTy,
        callee: Option<&str>,
        arg: Ty,
        recursive_call: bool,
    ) -> CompTy {
        let given = &mut self.calls[call.0 as usize].0;
        let index = *given;
        *given += 1;
        let result = self.fresh_comp();
        self.constrain(|this| {
            this.suspend(Match::Argument {
                call,
                index,
                head: head.clone(),
                callee: callee.map(str::to_owned),
                arg,
                result: result.clone(),
                recursive_call,
            });
        });
        result
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "constraint API owns provenance"
    )]
    pub(super) fn weak(&mut self, ty: &Ty, source: WeakSource) {
        self.constrain(|this| this.unifier.mark_weak(ty, &source));
    }

    pub(super) fn gate(&mut self, ty: &Ty, boundary: ArgvBoundary) {
        self.gates.push((self.unifier.at, ty.clone(), boundary));
    }

    pub(super) fn nested(&mut self, check: NestedCheck) {
        self.nested.push(check);
    }

    pub(super) fn unit_read(&mut self, call: UnitCall, ty: &Ty) {
        self.unit_reads.push((self.unifier.at, ty.clone(), call));
    }

    pub(super) fn diagnose(&mut self, kind: TypeErrorKind) {
        self.raise(TypeError {
            pos: self.unifier.at,
            kind,
            reason: None,
            weak: None,
            unit: None,
        });
    }

    /// Every public constraint is one operation followed by one propagation;
    /// the operations themselves never propagate.
    fn constrain<R>(&mut self, operation: impl FnOnce(&mut Self) -> R) -> R {
        let result = operation(self);
        self.propagate();
        result
    }

    fn unify(&mut self, a: &Ty, b: &Ty, why: Reason) -> bool {
        let outcome = self.unifier.unify_ty(a, b);
        self.held(outcome.map_err(|kind| Box::new((kind, why))), |u| {
            u.weak_source_in_ty(a).or_else(|| u.weak_source_in_ty(b))
        })
    }

    fn unify_comp(&mut self, a: &CompTy, b: &CompTy, why: Reason) -> bool {
        let outcome = self.unifier.unify_comp_ty(a, b);
        self.held(outcome.map_err(|kind| Box::new((kind, why))), |u| {
            u.weak_source_in_comp(a)
                .or_else(|| u.weak_source_in_comp(b))
        })
    }

    /// Whether an equation held; a refused one is reported with the weak
    /// variable it met.
    fn held(
        &mut self,
        outcome: Result<(), Clash>,
        weak: impl FnOnce(&Unifier) -> Option<WeakSource>,
    ) -> bool {
        let Err(clash) = outcome else {
            return true;
        };
        let (kind, why) = *clash;
        let weak = weak(&self.unifier);
        self.report(kind, why, weak);
        false
    }

    fn report(&mut self, kind: TypeErrorKind, why: Reason, weak: Option<WeakSource>) {
        let why = match why {
            Reason::CaseCoverage { scrutinee, arms } => Reason::CaseCoverage {
                scrutinee: self.unifier.apply_row(&scrutinee),
                arms,
            },
            why => why,
        };
        self.raise(TypeError {
            pos: self.unifier.at,
            kind: kind.reported(&why),
            reason: Some(why),
            weak,
            unit: None,
        });
    }

    fn raise(&mut self, error: TypeError) {
        let said = self.errors.iter().any(|previous| {
            previous.pos == error.pos
                && previous.kind.render_message() == error.kind.render_message()
        });
        if !said {
            self.errors.push(error);
        }
    }

    fn suspend(&mut self, constraint: Match) {
        let (Match::Binding {
            rhs: scrutinised, ..
        }
        | Match::Argument {
            head: scrutinised, ..
        }) = &constraint;
        self.unifier.remember_producers(Term::Comp(scrutinised));
        let pos = self.unifier.at;
        self.step(Suspended { constraint, pos });
    }

    /// Wake the producer pairs and matches on every root the store touched,
    /// until it touches none.
    fn propagate(&mut self) {
        let at = self.unifier.at;
        loop {
            let mut touched = self.unifier.take_touched();
            if touched.is_empty() {
                break;
            }
            touched.sort_unstable();
            touched.dedup();
            self.unifier.improve_producers(&touched);
            for root in touched {
                for pending in self.waiting.remove(&root).into_iter().flatten() {
                    self.unifier.at = pending.pos;
                    self.step(pending);
                }
            }
        }
        self.unifier.at = at;
    }

    /// Decide `pending` if its scrutinee's head allows; otherwise it waits on
    /// that root.
    fn step(&mut self, mut pending: Suspended) {
        let scrutinee = loop {
            match &mut pending.constraint {
                Match::Binding {
                    hole, rhs, binder, ..
                } => {
                    let head = self.unifier.resolve_comp_ty_graded(rhs);
                    let (mode, ty) = match head.node() {
                        CompTyNode::Fun(..) => (Mode::Thunk, Ty::thunk(head.clone())),
                        CompTyNode::Return(Grade::Value, ty) => (Mode::Run, ty.clone()),
                        CompTyNode::Return(Grade::Output, _) => (Mode::Capture, Ty::string()),
                        // A payload excluding Unit has already improved the grade.
                        CompTyNode::Return(Grade::Var(v), _) => break Var::Grade(*v),
                        CompTyNode::Var(v) => break Var::Comp(*v),
                    };
                    self.modes[hole.0 as usize] = Some(mode);
                    self.unify(binder, &ty, Reason::Binding { defaulted_at: None });
                    return;
                }
                Match::Argument {
                    call,
                    index,
                    head,
                    arg,
                    result,
                    recursive_call,
                    ..
                } => {
                    let why = Reason::Call {
                        defaulted_at: None,
                        recursive_call: *recursive_call,
                    };
                    let resolved = self.unifier.resolve_comp_ty_graded(head);
                    match resolved.node() {
                        CompTyNode::Var(v) => break Var::Comp(*v),
                        CompTyNode::Fun(param, body) => {
                            self.unify(arg, param, why.clone());
                            self.unify_comp(result, body, why);
                            return;
                        }
                        CompTyNode::Return(_, payload) => {
                            let next = self.fresh_comp();
                            if !self.unify(payload, &Ty::thunk(next.clone()), why) {
                                return;
                            }
                            self.calls[call.0 as usize].1.push(*index);
                            *head = next;
                        }
                    }
                }
            }
        };
        pending.lower(&mut self.unifier, scrutinee);
        self.waiting.entry(scrutinee).or_default().push(pending);
    }

    fn equation(&mut self, equation: Equation, why: Reason) {
        match equation {
            Equation::Value(a, b) => {
                self.unify(&a, &b, why);
            }
            Equation::Computation(a, b) => {
                self.unify_comp(&a, &b, why);
            }
        }
    }

    /// Default every match on a young scrutinee.  A scrutinee both bound and
    /// applied has no default and is refused instead.
    fn close_region(&mut self) {
        let rank = self.unifier.rank();
        let closing = self.unifier.at;
        let mut young: Vec<_> = self
            .waiting
            .extract_if(|&scrutinee, _| self.unifier.rank_of(scrutinee) == rank)
            .collect();
        young.sort_unstable_by_key(|&(scrutinee, _)| scrutinee);
        let mut equations = Vec::new();
        for (_, waiters) in young {
            let binding = waiters.iter().find_map(|w| match &w.constraint {
                Match::Binding { forced, .. } => Some((w.pos, forced)),
                Match::Argument { .. } => None,
            });
            let call = waiters.iter().find_map(|w| match &w.constraint {
                Match::Argument { callee, .. } => Some((w.pos, callee)),
                Match::Binding { .. } => None,
            });
            if let (Some((binding, forced)), Some((call, callee))) = (binding, call) {
                self.unifier.at = [binding, call]
                    .into_iter()
                    .flatten()
                    .min_by_key(|span| (span.file, span.start, span.end));
                self.diagnose(TypeErrorKind::ConflictingDefaults {
                    binding,
                    call,
                    forced: forced.clone(),
                    callee: callee.clone(),
                });
                continue;
            }
            for Suspended { constraint, pos } in waiters {
                match constraint {
                    Match::Binding {
                        hole, rhs, binder, ..
                    } => {
                        self.modes[hole.0 as usize] = Some(Mode::Run);
                        let payload = self.fresh_ty();
                        let why = Reason::Binding {
                            defaulted_at: closing,
                        };
                        equations.push((
                            pos,
                            why.clone(),
                            Equation::Computation(rhs, CompTy::pure(payload.clone())),
                        ));
                        equations.push((pos, why, Equation::Value(binder, payload)));
                    }
                    Match::Argument {
                        head,
                        arg,
                        result,
                        recursive_call,
                        ..
                    } => equations.push((
                        pos,
                        Reason::Call {
                            defaulted_at: closing,
                            recursive_call,
                        },
                        Equation::Computation(head, CompTy::arrows([arg], result)),
                    )),
                }
            }
        }
        // The whole default set is chosen before any of its equations is added.
        for (pos, why, equation) in equations {
            self.unifier.at = pos;
            self.equation(equation, why);
        }
        self.unifier.at = closing;
        self.propagate();
        let waiting = self.waiting.len();
        self.waiting
            .retain(|&scrutinee, _| self.unifier.rank_of(scrutinee) < rank);
        assert!(
            self.waiting.len() == waiting || !self.errors.is_empty(),
            "a successful region left a young match"
        );
    }

    /// The solution, unless the unit has errors; they are sorted by position.
    pub(super) fn close(mut self) -> Result<Solution, Vec<TypeError>> {
        assert_eq!(self.unifier.rank(), 0, "unit closed inside a region");
        self.close_region();
        for (pos, ty, boundary) in std::mem::take(&mut self.gates) {
            let head = self.unifier.resolve_ty(&ty);
            let Some(refused) = RefusedArg::of_ty(&head) else {
                continue;
            };
            let command = match boundary {
                ArgvBoundary::Exec(command) => command,
                ArgvBoundary::InShell(command) if refused == RefusedArg::Unit => command,
                ArgvBoundary::InShell(_) => continue,
            };
            self.unifier.at = pos;
            self.diagnose(TypeErrorKind::ExecArgNotText {
                command,
                ty: self.unifier.apply_ty(&ty),
                refused,
            });
        }
        let nested = contract::check_nested(&self.unifier, std::mem::take(&mut self.nested));
        self.errors.extend(nested);
        if !self.errors.is_empty() {
            for error in &mut self.errors {
                error.unit = self.unit_reads.iter().find_map(|(pos, ty, call)| {
                    (pos.is_some()
                        && *pos == error.pos
                        && matches!(self.unifier.resolve_ty(ty).node(), TyNode::Unit))
                    .then(|| call.clone())
                });
            }
            sort_errors(&mut self.errors);
            return Err(self.errors);
        }
        let modes = self
            .modes
            .into_iter()
            .collect::<Option<_>>()
            .expect("a unit without errors decides every binding");
        let splits = self
            .calls
            .into_iter()
            .map(|(_, mut splits)| {
                splits.sort_unstable();
                splits.into()
            })
            .collect();
        Ok(Solution {
            unifier: self.unifier,
            modes,
            splits,
        })
    }
}

impl Solution {
    pub(super) fn mode(&self, hole: BindHole) -> Mode {
        self.modes[hole.0 as usize]
    }

    /// Argument indices before which a returned block is forced; ascending, repeats allowed.
    pub(super) fn splits(&self, hole: CallHole) -> &[usize] {
        &self.splits[hole.0 as usize]
    }

    /// A copy of the store, for checks that must not constrain the solution.
    pub(super) fn scratch(&self) -> Unifier {
        self.unifier.clone()
    }

    pub(super) fn site(&self, ty: &Ty, unit: UnitId) -> Site {
        Site::snapshot(&self.unifier, ty, unit)
    }

    pub(super) fn settle(&self, scheme: Arc<Scheme>) -> Closed {
        settle_weak(&self.unifier, scheme)
    }

    pub(super) fn resolve_ty(&self, ty: &Ty) -> Ty {
        self.unifier.resolve_ty(ty)
    }

    pub(super) fn resolve_comp(&self, ty: &CompTy) -> CompTy {
        self.unifier.resolve_comp_ty_graded(ty)
    }
}

#[cfg(test)]
impl Solver {
    /// The store a refused unit leaves after its defaults, beside its errors.
    pub(super) fn refused(mut self) -> (Unifier, Vec<TypeError>) {
        self.close_region();
        let store = self.unifier.clone();
        let Err(errors) = self.close() else {
            panic!("the unit must be refused")
        };
        (store, errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::FileId;
    use crate::ty::{Role, TyVar};

    fn forall_tys(scheme: &Scheme) -> Vec<TyVar> {
        scheme
            .tys
            .iter()
            .filter_map(|(&v, role)| role.forall().map(|_| v))
            .collect()
    }

    fn at(start: u32) -> Span {
        Span::new(FileId::DUMMY, start, start + 1)
    }

    fn thunk(body: CompTy) -> Ty {
        Ty::thunk(body)
    }

    fn command() -> CompTy {
        CompTy::ret(Grade::Output, Ty::unit())
    }

    fn function() -> CompTy {
        CompTy::arrows([Ty::int()], command())
    }

    fn exit(solver: &mut Solver) {
        let rank = solver.unifier.rank();
        solver.exit();
        assert_eq!(solver.unifier.rank(), rank - 1);
        assert!(
            solver
                .waiting
                .keys()
                .all(|&scrutinee| solver.unifier.rank_of(scrutinee) < rank)
        );
    }

    fn success(solver: Solver) -> Solution {
        match solver.close() {
            Ok(solution) => solution,
            Err(errors) => panic!("{errors:?}"),
        }
    }

    fn apply(
        solver: &mut Solver,
        head: &CompTy,
        arg: Ty,
        recursive_call: bool,
    ) -> (CompTy, CallHole) {
        let call = solver.call();
        (solver.argument(call, head, None, arg, recursive_call), call)
    }

    fn waiters(solver: &Solver) -> usize {
        solver.waiting.values().map(Vec::len).sum()
    }

    #[test]
    fn nested_producer_grades_improve_after_payload_discovery_in_either_order() {
        for handles in [false, true] {
            for discover_first in [false, true] {
                let mut solver = Solver::new();
                solver.enter();
                let payload = solver.fresh_ty();
                let grade = solver.fresh_grade();
                let producer = if handles {
                    Ty::list(Ty::handle(grade, payload.clone()))
                } else {
                    thunk(CompTy::arrows(
                        [Ty::unit()],
                        CompTy::ret(grade, payload.clone()),
                    ))
                };
                if discover_first {
                    solver.equate(&payload, &Ty::int(), Reason::IfBranches);
                }
                let opaque = solver.opaque(&producer);
                if !discover_first {
                    solver.equate(&payload, &Ty::int(), Reason::IfBranches);
                }
                assert_eq!(solver.unifier.resolve_grade(grade), Grade::Value);
                exit(&mut solver);
                assert!(solver.generalise(&opaque).grades.is_empty());
                success(solver);
            }
        }
    }

    #[test]
    fn generalised_producer_pairs_are_improved_only_in_their_instances() {
        let mut solver = Solver::new();
        solver.enter();
        let payload = solver.fresh_ty();
        let grade = solver.fresh_grade();
        let opaque = solver.opaque(&thunk(CompTy::ret(grade, payload)));
        exit(&mut solver);
        let scheme = solver.generalise(&opaque);
        assert_eq!(scheme.grades.len(), 1);
        assert_eq!(forall_tys(&scheme).len(), 1);
        for concrete in [Ty::int(), Ty::unit()] {
            let instance = solver.instantiate(&scheme);
            let resolved = solver.unifier.resolve_ty(&instance);
            let TyNode::Thunk(body) = resolved.node() else {
                unreachable!()
            };
            let &CompTyNode::Return(instance_grade, ref instance_payload) = body.node() else {
                unreachable!()
            };
            solver.equate(instance_payload, &concrete, Reason::IfBranches);
            assert_eq!(
                solver.unifier.resolve_grade(instance_grade),
                if concrete == Ty::int() {
                    Grade::Value
                } else {
                    instance_grade
                },
            );
            assert_eq!(solver.unifier.resolve_grade(grade), grade);
        }
        success(solver);
    }

    #[test]
    fn young_producer_grades_generalise_independently_of_old_weak_payloads() {
        for handles in [false, true] {
            for output_first in [false, true] {
                let mut solver = Solver::new();
                let payload = solver.fresh_kinded(Kind::ANY);
                let source = WeakSource::Boundary("door".into());
                solver.weak(&payload, source.clone());
                solver.enter();
                let grade = solver.fresh_grade();
                let producer = |grade, payload| {
                    if handles {
                        Ty::handle(grade, payload)
                    } else {
                        thunk(CompTy::arrows([Ty::int()], CompTy::ret(grade, payload)))
                    }
                };
                let opaque = solver.opaque(&producer(grade, payload.clone()));
                exit(&mut solver);
                let scheme = solver.generalise(&opaque);
                assert_eq!(scheme.grades.len(), 1);
                assert_eq!(forall_tys(&scheme), []);
                let TyNode::Var(payload_var) = payload.node() else {
                    unreachable!()
                };
                assert_eq!(scheme.tys.get(payload_var), Some(&Role::Weak(Kind::ANY)));
                assert_eq!(solver.unifier.weak_source_in_ty(&payload), Some(source));

                solver.enter();
                let instances = [solver.instantiate(&scheme), solver.instantiate(&scheme)];
                let pairs = instances.each_ref().map(|instance| {
                    let applied = solver.unifier.apply_ty(instance);
                    match applied.node() {
                        TyNode::Handle(grade, payload) => (*grade, payload.clone()),
                        TyNode::Thunk(body) => {
                            let CompTyNode::Fun(arg, result) = body.node() else {
                                unreachable!()
                            };
                            assert_eq!(*arg, Ty::int());
                            let CompTyNode::Return(grade, payload) = result.node() else {
                                unreachable!()
                            };
                            (*grade, payload.clone())
                        }
                        _ => unreachable!(),
                    }
                });
                assert_ne!(pairs[0].0, pairs[1].0);
                assert!(pairs.iter().all(|(g, p)| *g != grade && *p == payload));
                let grades = if output_first {
                    [Grade::Output, Grade::Value]
                } else {
                    [Grade::Value, Grade::Output]
                };
                for (instance, concrete) in instances.iter().zip(grades) {
                    assert!(solver.equate(
                        instance,
                        &producer(concrete, Ty::unit()),
                        Reason::IfBranches,
                    ));
                }
                assert_eq!(solver.unifier.resolve_ty(&payload), Ty::unit());
                for ((instance_grade, _), concrete) in pairs.iter().zip(grades) {
                    assert_eq!(solver.unifier.resolve_grade(*instance_grade), concrete);
                }
                assert_eq!(solver.unifier.resolve_grade(grade), grade);
                let settled = settle_weak(&solver.unifier, Arc::new(scheme.clone()));
                assert_eq!(settled.grades, scheme.grades);
                assert_eq!(settled.ty, producer(grade, Ty::unit()));
                exit(&mut solver);
                assert_eq!(solver.generalise(&opaque).grades, scheme.grades);
                success(solver);
            }
        }
    }

    #[test]
    fn weak_roots_still_lower_grades_inside_returned_thunks() {
        for handles in [false, true] {
            let mut solver = Solver::new();
            let root = solver.fresh_ty();
            let source = WeakSource::Boundary("door".into());
            solver.weak(&root, source.clone());
            solver.enter();
            let grade = solver.fresh_grade();
            let payload = solver.fresh_ty();
            let producer = if handles {
                Ty::handle(grade, payload.clone())
            } else {
                thunk(CompTy::ret(grade, payload.clone()))
            };
            assert!(solver.equate(&root, &producer, Reason::Argument));
            let Grade::Var(grade_var) = grade else {
                unreachable!()
            };
            assert_eq!(solver.unifier.rank_of(Var::Grade(grade_var)), 0);
            assert_eq!(solver.unifier.weak_source_in_ty(&payload), Some(source));
            exit(&mut solver);
            let scheme = solver.generalise(&root);
            assert!(scheme.grades.is_empty());
            assert_eq!(forall_tys(&scheme), []);
            success(solver);
        }
    }

    #[test]
    fn fresh_alias_and_bound_accept_a_term_generated_deep_arrow_spine() {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(|| {
                let mut solver = Solver::new();
                solver.enter();
                let rhs = CompTy::arrows((0..2_000).map(|_| Ty::int()), CompTy::pure(Ty::int()));
                let ty = thunk(rhs.clone());
                let opaque = solver.opaque(&ty);
                let (binder, _) = solver.binding(&rhs, None);
                exit(&mut solver);
                let solution = success(solver);
                assert_eq!(solution.resolve_ty(&opaque), ty);
                assert_eq!(solution.resolve_ty(&binder), ty);
            })
            .expect("spawn deep-stack thread")
            .join()
            .expect("deep-arrow assertion");
    }

    #[test]
    fn type_nesting_too_deep_by_doubling_reports_an_error_without_a_wrapper_panic() {
        use crate::compile::{CompileError, compile_and_typecheck};

        let source = include_str!("../../../tests/reject/type-nesting-too-deep-by-doubling.ral");
        let result = compile_and_typecheck(
            source,
            crate::test_helper::core_schemes(),
            FileId::DUMMY,
            None,
            None,
        );
        let Err(CompileError::Types(errors)) = result else {
            panic!("doubling must report a type error, got {result:?}");
        };
        assert!(
            errors
                .iter()
                .any(|error| matches!(error.kind, TypeErrorKind::TypeTooDeep))
        );
    }

    #[test]
    fn bound_and_if_command_transpose_to_capture() {
        // let y = !$f; if true { !$f } else { echo a }, in either order.
        for context_first in [false, true] {
            let mut solver = Solver::new();
            solver.enter();
            let f = solver.fresh_comp();
            if context_first {
                solver.equate_comp(&f, &command(), Reason::IfBranches);
            }
            solver.enter();
            let (y, hole) = solver.binding(&f, None);
            assert!(matches!(y.node(), TyNode::Var(_)));
            exit(&mut solver);
            if !context_first {
                solver.equate_comp(&f, &command(), Reason::IfBranches);
            }
            exit(&mut solver);
            let solution = success(solver);
            assert_eq!(solution.mode(hole), Mode::Capture);
            assert_eq!(solution.resolve_ty(&y), Ty::string());
        }
    }

    #[test]
    fn apply_and_if_returned_function_transpose_to_one_split() {
        // f 1; if true { !$f } else { return { |n| echo $n } }.
        for context_first in [false, true] {
            let mut solver = Solver::new();
            solver.enter();
            let f = solver.fresh_comp();
            let context = CompTy::pure(thunk(function()));
            if context_first {
                solver.equate_comp(&f, &context, Reason::IfBranches);
            }
            let (result, call) = apply(&mut solver, &f, Ty::int(), false);
            assert!(matches!(result.node(), CompTyNode::Var(_)));
            if !context_first {
                solver.equate_comp(&f, &context, Reason::IfBranches);
            }
            exit(&mut solver);
            let solution = success(solver);
            assert_eq!(solution.splits(call), [0]);
            assert_eq!(solution.resolve_comp(&result), command());
        }
    }

    #[test]
    fn echo_and_list_unit_transpose_to_final_gate_refusal() {
        // echo $x; return [$x, ()].
        for context_first in [false, true] {
            let mut solver = Solver::new();
            let x = solver.fresh_ty();
            if context_first {
                solver.equate(&x, &Ty::unit(), Reason::ListElem);
            }
            solver.gate(&x, ArgvBoundary::InShell("echo".into()));
            if !context_first {
                solver.equate(&x, &Ty::unit(), Reason::ListElem);
            }
            let errors = solver.refused().1;
            assert_eq!(errors.len(), 1);
            assert!(matches!(
                errors[0].kind,
                TypeErrorKind::ExecArgNotText {
                    refused: RefusedArg::Unit,
                    ..
                }
            ));
        }
    }

    fn conflicting_uses(solver: &mut Solver, reverse: bool) -> (CompTy, Ty, CompTy) {
        let f = solver.fresh_comp();
        let alias = solver.fresh_comp();
        solver.equate_comp(&f, &alias, Reason::Argument);
        let (binder, result) = if reverse {
            solver.unifier.at = Some(at(20));
            let (result, _) = apply(solver, &alias, Ty::int(), false);
            solver.unifier.at = Some(at(10));
            (solver.binding(&f, None).0, result)
        } else {
            solver.unifier.at = Some(at(10));
            let (binder, _) = solver.binding(&f, None);
            solver.unifier.at = Some(at(20));
            (binder, apply(solver, &alias, Ty::int(), false).0)
        };
        (f, binder, result)
    }

    fn assert_conflict(errors: &[TypeError]) {
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            matches!(errors[0].kind, TypeErrorKind::ConflictingDefaults { binding, call, .. }
            if binding == Some(at(10)) && call == Some(at(20)))
        );
        assert_eq!(errors[0].pos, Some(at(10)));
    }

    #[test]
    fn conflicting_defaults_at_rank_zero_in_both_orders_choose_no_branch() {
        for reverse in [false, true] {
            let mut solver = Solver::new();
            let (f, binder, result) = conflicting_uses(&mut solver, reverse);
            let (store, errors) = solver.refused();
            assert_conflict(&errors);
            assert!(matches!(
                store.resolve_comp_ty_graded(&f).node(),
                CompTyNode::Var(_)
            ));
            assert!(matches!(store.resolve_ty(&binder).node(), TyNode::Var(_)));
            assert!(matches!(
                store.resolve_comp_ty_graded(&result).node(),
                CompTyNode::Var(_)
            ));
            assert_eq!(store.rank(), 0);
        }
    }

    #[test]
    fn conflicting_defaults_in_nested_region_retire_before_sibling_in_both_orders() {
        for reverse in [false, true] {
            let mut solver = Solver::new();
            solver.enter();
            solver.enter();
            let (f, binder, _) = conflicting_uses(&mut solver, reverse);
            exit(&mut solver);
            assert_conflict(&solver.errors);
            assert!(solver.waiting.is_empty());
            assert!(matches!(
                solver.unifier.resolve_comp_ty_graded(&f).node(),
                CompTyNode::Var(_)
            ));
            assert!(matches!(
                solver.unifier.resolve_ty(&binder).node(),
                TyNode::Var(_)
            ));
            solver.enter();
            let sibling = solver.fresh_comp();
            solver.binding(&sibling, None);
            exit(&mut solver);
            assert!(matches!(
                solver.unifier.resolve_comp_ty_graded(&sibling).node(),
                CompTyNode::Return(Grade::Value, _)
            ));
            exit(&mut solver);
            assert_conflict(&solver.refused().1);
        }
    }

    #[test]
    fn explicit_function_and_returned_function_spellings_stay_distinct() {
        for returned in [false, true] {
            for reverse in [false, true] {
                let mut solver = Solver::new();
                solver.enter();
                let f = solver.fresh_comp();
                let head = if returned {
                    solver.fresh_comp()
                } else {
                    f.clone()
                };
                let call = solver.call();
                if reverse {
                    solver.argument(call, &head, None, Ty::int(), false);
                }
                solver.enter();
                // let a = $f, or let h = !$f; h 1.
                let rhs = if returned {
                    f.clone()
                } else {
                    CompTy::pure(thunk(f.clone()))
                };
                let (binder, hole) = solver.binding(&rhs, None);
                exit(&mut solver);
                if returned {
                    solver.equate(
                        &binder,
                        &thunk(head.clone()),
                        Reason::Head {
                            name: Some("h".into()),
                        },
                    );
                }
                if !reverse {
                    solver.argument(call, &head, None, Ty::int(), false);
                }
                exit(&mut solver);
                let solution = success(solver);
                assert_eq!(solution.splits(call), [] as [usize; 0]);
                assert_eq!(solution.mode(hole), Mode::Run);
                if returned {
                    assert!(matches!(
                        solution.resolve_comp(&f).node(),
                        CompTyNode::Return(Grade::Value, _)
                    ));
                } else {
                    assert!(matches!(
                        solution.resolve_comp(&f).node(),
                        CompTyNode::Fun(..)
                    ));
                }
            }
        }
    }

    #[test]
    fn app2_captures_for_both_mk_bodies_and_constraint_orders() {
        // app2 { |f| let y = !$f; return $y } { [let z = 1;] return { echo hi } }.
        for local_let in [false, true] {
            for context_first in [false, true] {
                let mut solver = Solver::new();
                solver.enter();
                let f = solver.fresh_comp();
                let k = CompTy::arrows([thunk(f.clone())], CompTy::pure(Ty::string()));
                let mk = CompTy::pure(thunk(command()));
                if local_let {
                    solver.enter();
                    solver.binding(&CompTy::pure(Ty::int()), None);
                    exit(&mut solver);
                }
                let (argument, _) = solver.binding(&mk, None);
                if context_first {
                    apply(&mut solver, &k, argument.clone(), false);
                }
                solver.enter();
                let (y, hole) = solver.binding(&f, None);
                exit(&mut solver);
                if !context_first {
                    apply(&mut solver, &k, argument, false);
                }
                exit(&mut solver);
                let solution = success(solver);
                assert_eq!(solution.mode(hole), Mode::Capture);
                assert_eq!(solution.resolve_ty(&y), Ty::string());
            }
        }
    }

    #[test]
    fn unit_context_refuses_at_bound_not_at_later_use() {
        // if true { !$f } else { return () }; let y = !$f; y 1 / upper $y.
        for application in [false, true] {
            for context_first in [false, true] {
                let mut solver = Solver::new();
                solver.enter();
                let f = solver.fresh_comp();
                let grade = solver.fresh_grade();
                let context = CompTy::ret(grade, Ty::unit());
                if context_first {
                    solver.equate_comp(&f, &context, Reason::IfBranches);
                }
                solver.enter();
                solver.unifier.at = Some(at(10));
                let (y, _) = solver.binding(&f, None);
                exit(&mut solver);
                if application {
                    let body = solver.fresh_comp();
                    solver.equate(
                        &y,
                        &thunk(body.clone()),
                        Reason::Head {
                            name: Some("y".into()),
                        },
                    );
                    apply(&mut solver, &body, Ty::int(), false);
                } else {
                    solver.equate(&y, &Ty::string(), Reason::Argument);
                }
                if !context_first {
                    solver.equate_comp(&f, &context, Reason::IfBranches);
                }
                solver.unifier.at = Some(at(30));
                exit(&mut solver);
                let errors = solver.refused().1;
                assert!(errors.iter().any(|error| error.pos == Some(at(10))
                    && matches!(error.reason, Some(Reason::Binding { defaulted_at }) if defaulted_at == Some(at(30)))));
            }
        }
    }

    #[test]
    fn each_unit_is_refused_and_map_command_is_captured_in_both_orders() {
        for map in [false, true] {
            for context_first in [false, true] {
                let mut solver = Solver::new();
                solver.enter();
                let f = solver.fresh_comp();
                let context = if map {
                    function()
                } else {
                    CompTy::pure(Ty::unit())
                };
                if context_first {
                    solver.equate_comp(&f, &context, Reason::Argument);
                }
                solver.enter();
                // map { |f| let y = f 1; return $y } [{ |n| echo $n }]
                // each { |t| let y = !$t; echo $y } [{ return () }]
                let rhs = if map {
                    apply(&mut solver, &f, Ty::int(), false).0
                } else {
                    f.clone()
                };
                let (y, hole) = solver.binding(&rhs, None);
                exit(&mut solver);
                if !map {
                    solver.gate(&y, ArgvBoundary::InShell("echo".into()));
                }
                if !context_first {
                    solver.equate_comp(&f, &context, Reason::Argument);
                }
                exit(&mut solver);
                if map {
                    let solution = success(solver);
                    assert_eq!(solution.mode(hole), Mode::Capture);
                    assert_eq!(solution.resolve_ty(&y), Ty::string());
                } else {
                    let errors = solver.refused().1;
                    assert_eq!(errors.len(), 1);
                    assert!(matches!(
                        errors[0].kind,
                        TypeErrorKind::ExecArgNotText { .. }
                    ));
                }
            }
        }
    }

    #[test]
    fn bound_improves_only_from_non_unit_head_or_kind_in_both_orders() {
        for kind in [false, true] {
            for context_first in [false, true] {
                let mut solver = Solver::new();
                let payload = solver.fresh_ty();
                let grade = solver.fresh_grade();
                let rhs = CompTy::ret(grade, payload.clone());
                let excludes_unit = if kind {
                    solver.fresh_kinded(Kind::NUMBER)
                } else {
                    Ty::int()
                };
                if context_first {
                    solver.equate(&payload, &excludes_unit, Reason::Argument);
                }
                let (binder, hole) = solver.binding(&rhs, None);
                if !context_first {
                    solver.equate(&payload, &excludes_unit, Reason::Argument);
                }
                assert_eq!(solver.unifier.resolve_grade(grade), Grade::Value);
                assert!(solver.waiting.is_empty());
                let solution = success(solver);
                assert_eq!(solution.mode(hole), Mode::Run);
                assert_eq!(solution.resolve_ty(&binder), solution.resolve_ty(&payload));
            }
        }
    }

    #[test]
    fn bound_never_improves_backwards_from_binder() {
        for payload in [Ty::unit(), Ty::int()] {
            let mut solver = Solver::new();
            let grade = solver.fresh_grade();
            let rhs = CompTy::ret(grade, Ty::unit());
            let (binder, _) = solver.binding(&rhs, None);
            solver.equate(&binder, &payload, Reason::Argument);
            assert!(matches!(solver.unifier.resolve_grade(grade), Grade::Var(_)));
            assert_eq!(waiters(&solver), 1);
            assert_eq!(solver.close().is_ok(), payload == Ty::unit());
        }
        let mut solver = Solver::new();
        let grade = solver.fresh_grade();
        let rhs = CompTy::ret(grade, Ty::unit());
        let (binder, _) = solver.binding(&rhs, None);
        solver.equate(&binder, &Ty::string(), Reason::Argument);
        let (store, _) = solver.refused();
        assert_eq!(store.resolve_grade(grade), Grade::Value);
    }

    #[test]
    fn failed_default_retires_cyclic_application_at_zero_and_nested_ranks() {
        for nested in [false, true] {
            let mut solver = Solver::new();
            if nested {
                solver.enter();
            }
            let head = solver.fresh_comp();
            solver.unifier.at = Some(at(10));
            let (result, _) = apply(&mut solver, &head, Ty::int(), false);
            solver.equate_comp(&result, &head, Reason::Argument);
            solver.unifier.at = Some(at(30));
            if nested {
                exit(&mut solver);
                assert!(solver.waiting.is_empty());
                assert_eq!(solver.unifier.rank(), 0);
            }
            let errors = solver.refused().1;
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert!(matches!(errors[0].kind, TypeErrorKind::CyclicType { .. }));
            assert!(
                matches!(errors[0].reason, Some(Reason::Call { defaulted_at, recursive_call: false }) if defaulted_at == Some(at(30)))
            );
        }
    }

    #[test]
    fn recursive_application_provenance_survives_suspension_and_returned_block_splits() {
        for recursive_call in [false, true] {
            let mut solver = Solver::new();
            let head = solver.fresh_comp();
            solver.unifier.at = Some(at(10));
            apply(&mut solver, &head, Ty::string(), recursive_call);
            solver.unifier.at = Some(at(30));
            let body = CompTy::arrows([Ty::int()], CompTy::pure(Ty::unit()));
            solver.equate_comp(&head, &CompTy::pure(thunk(body)), Reason::Argument);
            let errors = solver.refused().1;
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert!(matches!(errors[0].kind, TypeErrorKind::TyMismatch { .. }));
            assert_eq!(errors[0].pos, Some(at(10)));
            assert!(matches!(
                errors[0].reason,
                Some(Reason::Call { defaulted_at: None, recursive_call: found })
                    if found == recursive_call
            ));
        }
    }

    #[test]
    fn recursive_application_defaults_keep_syntactic_provenance_and_argument_span() {
        for nested in [false, true] {
            let mut solver = Solver::new();
            if nested {
                solver.enter();
            }
            let head = solver.fresh_comp();
            for (pos, body) in [(at(10), CompTy::pure(Ty::int())), (at(20), command())] {
                solver.unifier.at = Some(pos);
                let (result, _) = apply(&mut solver, &head, Ty::unit(), true);
                assert!(solver.equate_comp(&result, &body, Reason::IfBranches));
            }
            solver.unifier.at = Some(at(30));
            if nested {
                exit(&mut solver);
            }
            let errors = solver.refused().1;
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert!(matches!(
                errors[0].kind,
                TypeErrorKind::CompTyMismatch { .. }
            ));
            assert!(matches!(errors[0].reason,
                Some(Reason::Call { defaulted_at, recursive_call: true })
                    if defaulted_at == Some(at(30))
            ));
            assert_eq!(errors[0].pos, Some(at(20)));
            let hint = errors[0].hint().expect("recursive and default guidance");
            assert!(hint.contains("a definition that calls itself"), "{hint}");
            assert!(
                hint.contains("so the application calls it as a function"),
                "{hint}"
            );
        }
    }

    #[test]
    fn failed_apply_default_reports_once_without_replaying_or_losing_other_matches() {
        for reverse in [false, true] {
            let mut solver = Solver::new();
            let old = solver.fresh_comp();
            solver.enter();
            let (old_binder, old_hole) = solver.binding(&old, None);
            solver.equate(&old_binder, &Ty::string(), Reason::Argument);

            let head = solver.fresh_comp();
            let bodies = if reverse {
                [command(), CompTy::pure(Ty::int())]
            } else {
                [CompTy::pure(Ty::int()), command()]
            };
            for (pos, body) in [(at(10), &bodies[0]), (at(20), &bodies[1])] {
                solver.unifier.at = Some(pos);
                let (result, _) = apply(&mut solver, &head, Ty::unit(), false);
                assert!(solver.equate_comp(&result, body, Reason::IfBranches));
            }

            let rhs = solver.fresh_comp();
            let (binder, _) = solver.binding(&rhs, None);
            solver.equate(&binder, &Ty::bool(), Reason::Argument);
            let other_head = solver.fresh_comp();
            let (other_result, _) = apply(&mut solver, &other_head, Ty::int(), false);
            solver.equate_comp(&other_result, &command(), Reason::IfBranches);
            solver.unifier.at = Some(at(60));
            exit(&mut solver);

            assert_eq!(solver.errors.len(), 1, "{:?}", solver.errors);
            let error = &solver.errors[0];
            assert_eq!(error.pos, Some(at(20)));
            assert!(
                matches!(&error.kind, TypeErrorKind::CompTyMismatch { expected, actual }
                if *expected == bodies[0] && *actual == bodies[1])
            );
            assert!(
                matches!(error.reason, Some(Reason::Call { defaulted_at, recursive_call: false })
                if defaulted_at == Some(at(60)))
            );
            assert_eq!(solver.unifier.apply_comp_ty(&rhs), CompTy::pure(Ty::bool()));
            assert_eq!(solver.unifier.resolve_ty(&binder), Ty::bool());
            assert_eq!(
                solver.unifier.apply_comp_ty(&other_head),
                CompTy::arrows([Ty::int()], command())
            );
            assert_eq!(waiters(&solver), 1);
            assert!(solver.waiting.values().flatten().all(|pending| matches!(
                &pending.constraint,
                Match::Binding { hole, .. } if *hole == old_hole
            )));

            let (store, errors) = solver.refused();
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert_eq!(store.apply_comp_ty(&old), CompTy::pure(Ty::string()));
            assert_eq!(store.resolve_ty(&old_binder), Ty::string());
        }
    }

    #[test]
    fn all_young_defaults_are_fixed_even_when_another_default_fails() {
        for reverse in [false, true] {
            let mut solver = Solver::new();
            solver.enter();
            let grade = solver.fresh_grade();
            let rhs = CompTy::ret(grade, Ty::unit());
            let head = solver.fresh_comp();
            if reverse {
                apply(&mut solver, &head, Ty::int(), false);
            }
            let (binder, _) = solver.binding(&rhs, None);
            solver.equate(&binder, &thunk(head.clone()), Reason::Argument);
            if !reverse {
                apply(&mut solver, &head, Ty::int(), false);
            }
            exit(&mut solver);
            assert!(solver.waiting.is_empty());
            assert!(!solver.errors.is_empty());
            assert_eq!(solver.unifier.resolve_grade(grade), Grade::Value);
            assert!(matches!(
                solver.unifier.resolve_comp_ty_graded(&head).node(),
                CompTyNode::Fun(..)
            ));
            assert!(solver.close().is_err());
        }
    }

    #[test]
    fn old_matches_survive_successful_and_failed_inner_exits() {
        let mut solver = Solver::new();
        let old = solver.fresh_comp();
        solver.enter();
        solver.binding(&old, None);
        exit(&mut solver);
        assert_eq!(waiters(&solver), 1);
        solver.enter();
        conflicting_uses(&mut solver, false);
        exit(&mut solver);
        assert_eq!(waiters(&solver), 1);
        assert_eq!(solver.unifier.rank(), 0);
        solver.enter();
        exit(&mut solver);
        assert_eq!(waiters(&solver), 1);
        let (store, errors) = solver.refused();
        assert_conflict(&errors);
        assert!(matches!(
            store.resolve_comp_ty_graded(&old).node(),
            CompTyNode::Return(Grade::Value, _)
        ));
    }

    #[test]
    fn a_match_whose_scrutinee_grows_old_lowers_all_its_terms() {
        let mut solver = Solver::new();
        let old = solver.fresh_comp();
        solver.enter();
        let head = solver.fresh_comp();
        let arg = solver.fresh_ty();
        let (result, call) = apply(&mut solver, &head, arg.clone(), false);
        let (binder, hole) = solver.binding(&old, None);
        solver.equate(&binder, &thunk(head.clone()), Reason::Argument);
        for term in [&arg, &binder] {
            let TyNode::Var(v) = term.node() else {
                panic!("constraint answer must be unknown")
            };
            assert_eq!(solver.unifier.rank_of(Var::Ty(*v)), 0);
        }
        let &CompTyNode::Var(v) = result.node() else {
            panic!("application answer must be unknown")
        };
        assert_eq!(solver.unifier.rank_of(Var::Comp(v)), 0);
        exit(&mut solver);
        assert_eq!(waiters(&solver), 2);
        let solution = success(solver);
        assert_eq!(solution.mode(hole), Mode::Run);
        assert_eq!(solution.splits(call), [] as [usize; 0]);
    }

    #[test]
    fn successful_closure_decides_all_holes_including_repeated_splits() {
        let mut solver = Solver::new();
        solver.enter();
        let unknown = solver.fresh_comp();
        let grade = solver.fresh_grade();
        let holes = [
            (unknown, Mode::Run),
            (CompTy::ret(grade, Ty::unit()), Mode::Run),
            (function(), Mode::Thunk),
            (command(), Mode::Capture),
        ]
        .map(|(rhs, mode)| (solver.binding(&rhs, None).1, mode));
        let head = CompTy::pure(thunk(CompTy::pure(thunk(CompTy::arrows(
            [Ty::int(), Ty::string()],
            CompTy::pure(Ty::bool()),
        )))));
        let call = solver.call();
        let result = solver.argument(call, &head, None, Ty::int(), false);
        solver.argument(call, &result, None, Ty::string(), false);
        let (_, single) = apply(&mut solver, &function(), Ty::int(), false);
        exit(&mut solver);
        assert!(solver.waiting.is_empty());
        let solution = success(solver);
        for (hole, mode) in holes {
            assert_eq!(solution.mode(hole), mode);
        }
        assert_eq!(solution.splits(call), [0, 0]);
        assert_eq!(solution.splits(single), [] as [usize; 0]);
    }

    #[test]
    fn pure_return_thunk_cycles_fail_without_infinite_peeling() {
        let mut solver = Solver::new();
        let head = solver.fresh_comp();
        apply(&mut solver, &head, Ty::int(), false);
        assert!(!solver.equate_comp(&head, &CompTy::pure(thunk(head.clone())), Reason::Argument));
        let errors = solver.refused().1;
        assert!(
            errors
                .iter()
                .any(|error| matches!(error.kind, TypeErrorKind::CyclicType { .. }))
        );
    }

    #[test]
    fn gate_keeps_free_runtime_question_and_shell_only_refuses_unit() {
        let mut solver = Solver::new();
        let unknown = solver.fresh_ty();
        solver.gate(&unknown, ArgvBoundary::Exec("external".into()));
        solver.gate(&thunk(command()), ArgvBoundary::InShell("echo".into()));
        success(solver);
        let mut solver = Solver::new();
        solver.gate(&thunk(command()), ArgvBoundary::Exec("external".into()));
        let errors = solver.refused().1;
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            errors[0].kind,
            TypeErrorKind::ExecArgNotText { .. }
        ));
    }

    #[test]
    fn closure_errors_are_stable_source_order_and_unit_notes_use_final_types() {
        let mut solver = Solver::new();
        let unit = solver.fresh_ty();
        let not_unit = solver.fresh_ty();
        for (pos, ty, name) in [(Some(at(10)), &unit, "u"), (Some(at(30)), &not_unit, "n")] {
            solver.unifier.at = pos;
            solver.unit_read(
                UnitCall {
                    name: name.into(),
                    callee: "call".into(),
                },
                ty,
            );
        }
        solver.unifier.at = Some(at(30));
        solver.equate(&not_unit, &Ty::int(), Reason::Argument);
        solver.equate(&not_unit, &Ty::string(), Reason::Argument);
        solver.equate(&Ty::bool(), &Ty::string(), Reason::Argument);
        solver.unifier.at = Some(at(10));
        solver.gate(&unit, ArgvBoundary::InShell("echo".into()));
        solver.equate(&unit, &Ty::unit(), Reason::Argument);
        let errors = solver.refused().1;
        assert_eq!(errors.len(), 3);
        assert_eq!(errors[0].pos, Some(at(10)));
        assert_eq!(
            errors[0].unit.as_ref().map(|call| call.name.as_str()),
            Some("u")
        );
        assert!(errors[1].unit.is_none());
        assert!(errors[2].unit.is_none());
        assert!(matches!(
            &errors[1].kind,
            TypeErrorKind::TyMismatch { expected, .. } if *expected == Ty::int()
        ));
        assert!(matches!(
            &errors[2].kind,
            TypeErrorKind::TyMismatch { expected, .. } if *expected == Ty::bool()
        ));
    }

    #[test]
    fn weak_rank_zero_is_not_generalised_and_keeps_boundary_provenance() {
        let mut solver = Solver::new();
        solver.enter();
        let value = solver.fresh_ty();
        let source = WeakSource::Boundary(Arc::from("door"));
        solver.weak(&value, source.clone());
        exit(&mut solver);
        let scheme = solver.generalise(&value);
        assert_eq!(forall_tys(&scheme), []);
        solver.equate(&value, &Ty::int(), Reason::Argument);
        solver.equate(&value, &Ty::string(), Reason::Argument);
        let errors = solver.refused().1;
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].weak, Some(source));
    }

    /// The second use meets `Int`, which the weak variable was fixed to, not
    /// the variable itself.
    #[test]
    fn a_mismatch_with_what_a_weak_variable_was_fixed_to_carries_the_note() {
        let mut ctx = Solver::new();
        let weak = ctx.fresh_ty();
        ctx.weak(&weak, WeakSource::Boundary("from-json".into()));
        ctx.equate(&weak, &Ty::int(), Reason::Argument);
        ctx.equate(&weak, &Ty::string(), Reason::Argument);
        let [err] = &ctx.errors[..] else {
            panic!("one mismatch expected: {:?}", ctx.errors);
        };
        assert_eq!(err.weak, Some(WeakSource::Boundary("from-json".into())));
        assert!(
            err.hint()
                .is_some_and(|h| h.contains("another use fixed it"))
        );
    }

    /// One sentence is said once per span: a repeat is dropped, a new span or a
    /// new sentence is kept.
    #[test]
    fn raise_drops_only_the_same_sentence_at_the_same_span() {
        let mismatch = |actual| TypeErrorKind::TyMismatch {
            expected: Ty::int(),
            actual,
        };
        let at = |start| Some(Span::new(FileId::DUMMY, start, start + 1));
        let mut ctx = Solver::new();
        for (pos, ty) in [
            (at(0), Ty::string()),
            (at(0), Ty::string()),
            (at(2), Ty::string()),
            (at(0), Ty::bool()),
        ] {
            ctx.unifier.at = pos;
            ctx.diagnose(mismatch(ty));
        }
        let kept: Vec<_> = ctx.errors.iter().map(|e| e.pos).collect();
        assert_eq!(kept, [at(0), at(2), at(0)]);
    }
    #[test]
    fn nested_table_and_gate_checks_are_late_and_stably_source_ordered() {
        let mut solver = Solver::new();
        solver.unifier.at = Some(at(30));
        solver.gate(&Ty::unit(), ArgvBoundary::Exec("echo".into()));
        solver.nested(NestedCheck {
            key: "exec",
            table: contract::declared(contract::Form::Manifest),
            ty: Ty::int(),
            pos: Some(at(10)),
        });
        assert!(solver.errors.is_empty());
        let errors = solver.refused().1;
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].pos, Some(at(10)));
        assert!(matches!(errors[0].kind, TypeErrorKind::NotATable { .. }));
        assert_eq!(errors[1].pos, Some(at(30)));
        assert!(matches!(
            errors[1].kind,
            TypeErrorKind::ExecArgNotText { .. }
        ));
    }
}
