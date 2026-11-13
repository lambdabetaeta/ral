//! Type schemes (`forall alpha. A`): a type under universal quantifiers.
//!
//! `generalize.rs` builds these at `let` bindings and instantiates them with
//! fresh unification variables at each use site, giving let-polymorphism.

use super::kind::Kind;
use super::{
    CompTy, CompTyNode, CompTyVar, Grade, GradeVar, RowNode, RowVar, Term, Ty, TyNode, TyVar, Var,
};
use crate::fact::OpenScheme;
#[cfg(any(test, feature = "test-util"))]
use std::collections::HashSet;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

/// What a scheme's variable is: `M` is what minting a fresh one of its sort
/// needs, `B` its sort's μ-binding.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Role<M, B> {
    /// `∀`: minted fresh at each instantiation.
    Forall(M),
    /// One type for the whole unit that made the scheme, never quantified; a
    /// later unit reseeds it as a fresh weak variable
    /// (`generalize::reseed_weak`) instead of reading a foreign id.
    Weak(M),
    /// The root of a cycle, which cannot be a plain quantifier: instantiation
    /// mints a fresh root and binds it to this, so two instantiations never
    /// share the cycle's union-find slot.
    Mu(B),
}

impl<M: Copy, B> Role<M, B> {
    pub(crate) const fn is_weak(&self) -> bool {
        matches!(self, Self::Weak(_))
    }

    pub(crate) fn forall(&self) -> Option<M> {
        match *self {
            Self::Forall(meta) => Some(meta),
            Self::Weak(_) | Self::Mu(_) => None,
        }
    }

    pub(crate) const fn mu(&self) -> Option<&B> {
        match self {
            Self::Mu(binding) => Some(binding),
            Self::Forall(_) | Self::Weak(_) => None,
        }
    }

    /// This role with its μ-binding through `f`.
    pub(crate) fn map_mu<C>(&self, f: impl FnOnce(&B) -> C) -> Role<M, C> {
        match self {
            Self::Forall(meta) => Role::Forall(*meta),
            Self::Weak(meta) => Role::Weak(*meta),
            Self::Mu(binding) => Role::Mu(f(binding)),
        }
    }
}

/// Uninhabited: a cycle through a row is rooted at a type or a computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum NoMu {}

/// A polymorphic type scheme: `ty` under the roles of its variables.
///
/// One map per sort, so a role is always of its variable's sort.  A grade is
/// only ever quantified: settlement quantifies what is left of it.  Ordered,
/// so a scheme — and the letters it prints under — is a function of the type.
/// Keyed by bare ids, which JSON writes as string keys.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Scheme {
    pub(crate) ty: Ty,
    pub(crate) tys: BTreeMap<TyVar, Role<Kind, Ty>>,
    pub(crate) comps: BTreeMap<CompTyVar, Role<(), CompTy>>,
    /// A deep row's every field is `data`.
    pub(crate) rows: BTreeMap<RowVar, Role<bool, NoMu>>,
    pub(crate) grades: BTreeSet<GradeVar>,
}

impl Scheme {
    /// A scheme with nothing quantified.
    pub(crate) const fn mono(ty: Ty) -> Self {
        Self {
            ty,
            tys: BTreeMap::new(),
            comps: BTreeMap::new(),
            rows: BTreeMap::new(),
            grades: BTreeSet::new(),
        }
    }

    /// True when instantiation has work to do.  A μ-equation counts: a scheme
    /// with no quantifiers but a captured cycle still needs fresh roots.
    pub(crate) fn is_poly(&self) -> bool {
        !self.grades.is_empty()
            || self.tys.values().any(|role| !role.is_weak())
            || self.comps.values().any(|role| !role.is_weak())
            || self.rows.values().any(|role| !role.is_weak())
    }

    pub(crate) fn has_weak(&self) -> bool {
        self.weak().next().is_some()
    }

    /// A weak variable is reachable beneath a thunk or handle.
    pub(crate) fn delays_weak(&self) -> bool {
        let mut pending = vec![(Term::Ty(&self.ty), false)];
        let mut seen = BTreeSet::new();
        while let Some((term, delayed)) = pending.pop() {
            if term.is_ground() {
                continue;
            }
            let variable = match term {
                Term::Ty(ty) => match ty.node() {
                    TyNode::Var(v) => Some(Var::Ty(*v)),
                    _ => None,
                },
                Term::Comp(comp) => match comp.node() {
                    CompTyNode::Var(v) => Some(Var::Comp(*v)),
                    _ => None,
                },
                Term::Row(row) => match row.node() {
                    RowNode::Var(v) => Some(Var::Row(*v)),
                    _ => None,
                },
            };
            if let Some(variable) = variable {
                if !seen.insert((variable, delayed)) {
                    continue;
                }
                match variable {
                    Var::Ty(v) => match self.tys.get(&v) {
                        Some(Role::Weak(_)) if delayed => return true,
                        Some(Role::Mu(body)) => pending.push((Term::Ty(body), delayed)),
                        _ => {}
                    },
                    Var::Comp(v) => match self.comps.get(&v) {
                        Some(Role::Weak(())) if delayed => return true,
                        Some(Role::Mu(body)) => pending.push((Term::Comp(body), delayed)),
                        _ => {}
                    },
                    Var::Row(v) => {
                        if delayed && matches!(self.rows.get(&v), Some(Role::Weak(_))) {
                            return true;
                        }
                    }
                    Var::Grade(_) => {}
                }
                continue;
            }
            let delayed = delayed
                || matches!(term, Term::Ty(ty) if matches!(ty.node(), TyNode::Thunk(_) | TyNode::Handle(..)));
            pending.extend(term.children().map(|child| (child, delayed)));
        }
        false
    }

    /// Whether the declared final result is first-order data, including its open
    /// variables and μ-equations. Does not check the outer thunk or its arguments.
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn boundary_result_is_data(&self) -> bool {
        fn data(ty: &Ty, scheme: &Scheme, seen: &mut HashSet<TyVar>) -> bool {
            match ty.node() {
                TyNode::Unit
                | TyNode::Bytes
                | TyNode::Bool
                | TyNode::Int
                | TyNode::Float
                | TyNode::String => true,
                TyNode::List(elem) | TyNode::Map(elem) => data(elem, scheme, seen),
                TyNode::Record(fields) | TyNode::Variant(fields) => row(fields, scheme, seen),
                TyNode::Thunk(_) | TyNode::Handle(..) => false,
                TyNode::Var(v) => match scheme.tys.get(v) {
                    Some(Role::Forall(kind) | Role::Weak(kind)) => *kind <= Kind::Data,
                    Some(Role::Mu(binding)) => !seen.insert(*v) || data(binding, scheme, seen),
                    None => false,
                },
            }
        }

        fn row(fields: &super::Row, scheme: &Scheme, seen: &mut HashSet<TyVar>) -> bool {
            match fields.node() {
                RowNode::Empty => true,
                RowNode::Extend(_, ty, rest) => data(ty, scheme, seen) && row(rest, scheme, seen),
                RowNode::Var(v) => matches!(
                    scheme.rows.get(v),
                    Some(Role::Forall(true) | Role::Weak(true))
                ),
            }
        }

        let TyNode::Thunk(body) = self.ty.node() else {
            return false;
        };
        let mut body = body;
        let mut seen = HashSet::new();
        loop {
            match body.node() {
                CompTyNode::Fun(_, rest) => body = rest,
                CompTyNode::Return(_, ty) => return data(ty, self, &mut HashSet::new()),
                CompTyNode::Var(v) => {
                    if !seen.insert(*v) {
                        return false;
                    }
                    let Some(Role::Mu(binding)) = self.comps.get(v) else {
                        return false;
                    };
                    body = binding;
                }
            }
        }
    }

    /// The weak residuals, in sort order.
    pub(crate) fn weak(&self) -> impl Iterator<Item = Var> + '_ {
        picked(&self.tys, Var::Ty, Role::is_weak)
            .chain(picked(&self.comps, Var::Comp, Role::is_weak))
            .chain(picked(&self.rows, Var::Row, Role::is_weak))
    }

    /// The `∀`-bound variables, in sort order.
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn quantified(&self) -> impl Iterator<Item = Var> + '_ {
        picked(&self.tys, Var::Ty, |role| role.forall().is_some())
            .chain(picked(&self.comps, Var::Comp, |role| {
                role.forall().is_some()
            }))
            .chain(picked(&self.rows, Var::Row, |role| role.forall().is_some()))
            .chain(self.grades.iter().copied().map(Var::Grade))
    }

    /// Whether `var` has a role here.
    pub(crate) fn binds(&self, var: Var) -> bool {
        match var {
            Var::Ty(v) => self.tys.contains_key(&v),
            Var::Comp(v) => self.comps.contains_key(&v),
            Var::Row(v) => self.rows.contains_key(&v),
            Var::Grade(v) => self.grades.contains(&v),
        }
    }

    /// How many parameters a block of this scheme takes, read off its type.
    pub(crate) fn arity(&self) -> usize {
        match self.ty.node() {
            TyNode::Thunk(body) => body.arity(),
            _ => 0,
        }
    }

    /// Every variable written in the body and the μ-equations — syntactically,
    /// so no live slot can hide a dangling id by resolving it.
    pub(crate) fn syntactic_vars(&self) -> BTreeSet<Var> {
        let terms = std::iter::once(Term::Ty(&self.ty))
            .chain(self.tys.values().filter_map(Role::mu).map(Term::Ty))
            .chain(self.comps.values().filter_map(Role::mu).map(Term::Comp));
        let mut out = BTreeSet::new();
        for term in terms {
            collect_vars(term, &mut out);
        }
        out
    }

    /// The variables written here that have no role here: ones a unifier owns.
    pub(crate) fn foreign(&self) -> impl Iterator<Item = Var> + '_ {
        self.syntactic_vars()
            .into_iter()
            .filter(|&var| !self.binds(var))
    }

    /// What [`Closed::new`] checks.
    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        self.foreign().next().is_none()
    }
}

/// The variables of `vars` whose role `pick` admits, in order.
fn picked<V: Copy, M, B>(
    vars: &BTreeMap<V, Role<M, B>>,
    var: fn(V) -> Var,
    pick: fn(&Role<M, B>) -> bool,
) -> impl Iterator<Item = Var> + '_ {
    vars.iter()
        .filter(move |(_, role)| pick(role))
        .map(move |(&v, _)| var(v))
}

/// Every variable written in `term`, into `out`.
pub(crate) fn collect_vars(term: Term<'_>, out: &mut BTreeSet<Var>) {
    if term.is_ground() {
        return;
    }
    if let Some(Grade::Var(v)) = term.grade() {
        out.insert(Var::Grade(v));
    }
    let var = match term {
        Term::Ty(ty) => match ty.node() {
            TyNode::Var(v) => Some(Var::Ty(*v)),
            _ => None,
        },
        Term::Comp(cty) => match cty.node() {
            CompTyNode::Var(v) => Some(Var::Comp(*v)),
            _ => None,
        },
        Term::Row(row) => match row.node() {
            RowNode::Var(v) => Some(Var::Row(*v)),
            _ => None,
        },
    };
    out.extend(var);
    for child in term.children() {
        collect_vars(child, out);
    }
}

/// A scheme no unifier owns any part of: every variable it writes is one of
/// its own.  Only such a scheme leaves the unit that made it, since the next
/// unit's ids restart at zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Closed(Arc<Scheme>);

impl Closed {
    /// # Errors
    /// `scheme` writes a variable it gives no role.
    pub(crate) fn new(scheme: Arc<Scheme>) -> Result<Self, OpenScheme> {
        let foreign = scheme.foreign().next().map(|var| format!("{var:?}"));
        match foreign {
            Some(variable) => Err(OpenScheme {
                scheme: scheme.to_string(),
                variable,
            }),
            None => Ok(Self(scheme)),
        }
    }

    /// The scheme as a unit's environment holds it.
    pub(crate) fn scheme(&self) -> &Arc<Scheme> {
        &self.0
    }
}

impl Deref for Closed {
    type Target = Scheme;

    fn deref(&self) -> &Scheme {
        &self.0
    }
}

impl fmt::Display for Closed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl serde::Serialize for Closed {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for Closed {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(Arc::new(Scheme::deserialize(deserializer)?))
            .map_err(|open| serde::de::Error::custom(open.describe()))
    }
}

/// A builtin scheme's own binder supply, borrowing no inference session:
/// each binder distinct within its sort, and quantified.  Its body is the
/// rule's, set by [`Binders::close`].
#[derive(Debug)]
pub struct Binders(Scheme);

impl Binders {
    /// The scheme `rule`'s type is under the binders it drew.
    ///
    /// # Panics
    /// If the type writes a variable `rule` did not draw here.
    pub(crate) fn close(rule: impl FnOnce(&mut Self) -> Ty) -> Closed {
        let mut binders = Self(Scheme::mono(Ty::unit()));
        let ty = rule(&mut binders);
        Closed::new(Arc::new(Scheme { ty, ..binders.0 }))
            .unwrap_or_else(|open| panic!("builtin scheme: {}", open.describe()))
    }

    pub fn ty(&mut self, kind: Kind) -> TyVar {
        draw(&mut self.0.tys, TyVar, Role::Forall(kind))
    }

    pub fn comp(&mut self) -> CompTyVar {
        draw(&mut self.0.comps, CompTyVar, Role::Forall(()))
    }

    /// A deep row's every field is `data`.
    pub fn row(&mut self, deep: bool) -> RowVar {
        draw(&mut self.0.rows, RowVar, Role::Forall(deep))
    }

    pub fn grade(&mut self) -> GradeVar {
        let v = GradeVar(next_id(self.0.grades.len()));
        self.0.grades.insert(v);
        v
    }
}

/// A fresh binder of `vars`' sort, given `role`.
fn draw<V: Ord + Copy, R>(vars: &mut BTreeMap<V, R>, id: fn(u32) -> V, role: R) -> V {
    let v = id(next_id(vars.len()));
    vars.insert(v, role);
    v
}

/// Binders are drawn in order from zero, so their count is the next id.
fn next_id(drawn: usize) -> u32 {
    u32::try_from(drawn).expect("fewer than 2^32 binders")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ty::{Label, Row, closed_record};

    fn result(ty: Ty) -> Scheme {
        Scheme::mono(Ty::thunk(CompTy::pure(ty)))
    }

    #[test]
    fn boundary_data_check_ignores_arguments_and_unreachable_roles() {
        let scheme = Binders::close(|binders| {
            let any = Ty::var(binders.ty(Kind::Any));
            let shallow = Ty::record(Row::var(binders.row(false)));
            let computation = Ty::thunk(CompTy::var(binders.comp()));
            let _unused = binders.ty(Kind::Any);
            Ty::thunk(CompTy::arrows(
                [any, shallow, computation],
                CompTy::pure(Ty::int()),
            ))
        });
        assert!(scheme.boundary_result_is_data());
    }

    #[test]
    fn boundary_data_check_requires_data_kinds_for_all_result_variables() {
        let v = TyVar(0);
        for (kind, expected) in [
            (Kind::Data, true),
            (Kind::Scalar, true),
            (Kind::Number, true),
            (Kind::Comparable, true),
            (Kind::Any, false),
        ] {
            for role in [Role::Forall(kind), Role::Weak(kind)] {
                let mut scheme = result(Ty::map(Ty::list(Ty::var(v))));
                scheme.tys.insert(v, role);
                assert_eq!(scheme.boundary_result_is_data(), expected, "{scheme:?}");
            }
        }
        assert!(!result(Ty::var(v)).boundary_result_is_data());
    }

    #[test]
    fn boundary_data_check_requires_deep_rows_and_checks_their_fields() {
        let v = RowVar(0);
        for deep in [false, true] {
            for role in [Role::Forall(deep), Role::Weak(deep)] {
                for ty in [Ty::record(Row::var(v)), Ty::variant(Row::var(v))] {
                    let mut scheme = result(ty);
                    scheme.rows.insert(v, role.clone());
                    assert_eq!(scheme.boundary_result_is_data(), deep, "{scheme:?}");
                }
            }
        }
        for delayed in [
            Ty::thunk(CompTy::pure(Ty::int())),
            Ty::handle(Grade::Value, Ty::int()),
        ] {
            let record = Ty::record(Row::extend(
                Label::Field("value".into()),
                delayed.clone(),
                Row::empty(),
            ));
            let variant = Ty::variant(Row::extend(
                Label::Case("value".into()),
                delayed.clone(),
                Row::empty(),
            ));
            for ty in [
                delayed.clone(),
                Ty::list(delayed.clone()),
                Ty::map(delayed),
                record,
                variant,
            ] {
                assert!(!result(ty).boundary_result_is_data());
            }
        }
        assert!(!result(Ty::record(Row::var(v))).boundary_result_is_data());
    }

    #[test]
    fn boundary_data_check_follows_recursive_results_without_looping() {
        let v = TyVar(0);
        let mut scheme = result(Ty::var(v));
        scheme.tys.insert(v, Role::Mu(Ty::list(Ty::var(v))));
        assert!(scheme.boundary_result_is_data());
        scheme
            .tys
            .insert(v, Role::Mu(Ty::handle(Grade::Value, Ty::var(v))));
        assert!(!scheme.boundary_result_is_data());
    }

    #[test]
    fn boundary_data_check_follows_computation_equations_to_a_final_result() {
        let v = CompTyVar(0);
        let mut scheme = Scheme::mono(Ty::thunk(CompTy::var(v)));
        scheme.comps.insert(
            v,
            Role::Mu(CompTy::fun(Ty::int(), CompTy::pure(Ty::string()))),
        );
        assert!(scheme.boundary_result_is_data());
        scheme
            .comps
            .insert(v, Role::Mu(CompTy::fun(Ty::int(), CompTy::var(v))));
        assert!(!scheme.boundary_result_is_data());
    }

    fn recursive(ty: Ty) -> Scheme {
        let mut scheme = Scheme::mono(ty);
        scheme.tys.insert(
            TyVar(0),
            Role::Mu(closed_record(&[
                ("item", Ty::var(TyVar(1))),
                ("next", Ty::var(TyVar(0))),
            ])),
        );
        scheme.tys.insert(TyVar(1), Role::Weak(Kind::Data));
        scheme
    }

    #[test]
    fn delays_weak_follows_mu_equations_in_each_context() {
        let a = Ty::var(TyVar(0));
        let delayed = Ty::thunk(CompTy::pure(a.clone()));
        for (ty, expected) in [
            (delayed.clone(), true),
            (
                closed_record(&[("d", a.clone()), ("f", delayed.clone())]),
                true,
            ),
            (closed_record(&[("f", delayed), ("d", a.clone())]), true),
            (closed_record(&[("d", a)]), false),
            (Ty::int(), false),
        ] {
            let scheme = recursive(ty);
            assert_eq!(scheme.delays_weak(), expected, "{scheme}");
        }
    }

    #[test]
    fn delays_weak_checks_arguments_and_payloads_but_not_adjacent_data() {
        let weak = Ty::var(TyVar(1));
        for (ty, expected) in [
            (
                Ty::thunk(CompTy::fun(weak.clone(), CompTy::pure(Ty::unit()))),
                true,
            ),
            (Ty::handle(Grade::Value, weak.clone()), true),
            (
                closed_record(&[("x", weak), ("f", Ty::thunk(CompTy::pure(Ty::int())))]),
                false,
            ),
        ] {
            let mut scheme = Scheme::mono(ty);
            scheme.tys.insert(TyVar(1), Role::Weak(Kind::Data));
            assert_eq!(scheme.delays_weak(), expected, "{scheme}");
        }
    }

    #[test]
    fn delays_weak_includes_row_and_computation_residuals() {
        let mut row = Scheme::mono(Ty::thunk(CompTy::pure(Ty::record(Row::var(RowVar(0))))));
        row.rows.insert(RowVar(0), Role::Weak(true));
        assert!(row.delays_weak());
        row.ty = Ty::record(Row::var(RowVar(0)));
        assert!(!row.delays_weak());

        let mut comp = Scheme::mono(Ty::thunk(CompTy::var(CompTyVar(0))));
        comp.comps
            .insert(CompTyVar(0), Role::Mu(CompTy::var(CompTyVar(1))));
        comp.comps.insert(CompTyVar(1), Role::Weak(()));
        assert!(comp.delays_weak());
        comp.ty = Ty::int();
        assert!(!comp.delays_weak());
    }
}
