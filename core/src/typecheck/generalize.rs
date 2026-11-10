//! Generalization and instantiation for HM polymorphism.
//!
//! Equi-recursion is the twist: a cyclic type is anchored at a union-find
//! root, so a scheme snapshots that root's binding and `instantiate`
//! re-anchors it at a fresh root instead of sharing the original slot.

use super::unify::{Unifier, Visited};
use crate::ty::{
    Closed, CompTy, CompTyVar, Grade, GradeVar, Role, Row, RowVar, Scheme, Term, Ty, TyVar, Var,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::convert::Infallible;
use std::hash::Hash;
use std::ops::ControlFlow;
use std::sync::Arc;

/// The unbound roots `ty` reaches.
fn free_vars(u: &Unifier, ty: &Ty) -> BTreeSet<Var> {
    let mut out = BTreeSet::new();
    let _ = u.reach(Term::Ty(ty), &mut Visited::default(), &mut |var| {
        if u.is_unbound(var) {
            out.insert(var);
        }
        ControlFlow::<Infallible>::Continue(())
    });
    out
}

fn is_weak(u: &Unifier, var: Var) -> bool {
    match var {
        Var::Ty(v) => u.is_weak_ty(v),
        Var::Comp(v) => u.is_weak_comp(v),
        Var::Row(v) => u.is_weak_row(v),
        Var::Grade(_) => false,
    }
}

/// Gives `var` the role `u` says it has, weak or `∀`, with what a fresh
/// stand-in must keep: a type variable's kind, a row's deep bit.
fn bind(u: &Unifier, scheme: &mut Scheme, var: Var) {
    fn role<M, B>(weak: bool, meta: M) -> Role<M, B> {
        if weak {
            Role::Weak(meta)
        } else {
            Role::Forall(meta)
        }
    }
    let weak = is_weak(u, var);
    match var {
        Var::Ty(v) => {
            scheme.tys.insert(v, role(weak, u.kinded(v).kind));
        }
        Var::Comp(v) => {
            scheme.comps.insert(v, role(weak, ()));
        }
        Var::Row(v) => {
            scheme.rows.insert(v, role(weak, u.is_deep_row(v)));
        }
        Var::Grade(v) => {
            scheme.grades.insert(v);
        }
    }
}

/// Binds by μ each cycle `ty` reaches whose root `keep` admits and `scheme`
/// does not bind yet.
fn capture(u: &Unifier, ty: &Ty, scheme: &mut Scheme, keep: impl Fn(Var) -> bool) {
    let (comps, tys) = u.cyclic_roots_in_ty(ty);
    for root in comps.into_iter().filter(|&root| keep(Var::Comp(root))) {
        let binding = u
            .resolved_comp_root_binding(root)
            .unwrap_or(CompTy::Var(root));
        scheme.comps.entry(root).or_insert(Role::Mu(binding));
    }
    for root in tys.into_iter().filter(|&root| keep(Var::Ty(root))) {
        let binding = u.resolved_ty_root_binding(root).unwrap_or(Ty::Var(root));
        scheme.tys.entry(root).or_insert(Role::Mu(binding));
    }
}

pub(crate) fn generalize(u: &Unifier, ty: &Ty) -> Scheme {
    let applied = u.apply_ty(ty);
    let young = |var| u.is_young(var);
    // Kept weak anchors can already be bound; settlement still needs them.
    let mut scheme = Scheme::mono(u.apply_ty_keeping_weak(ty));
    capture(u, &applied, &mut scheme, young);
    // A cyclic root is bound by its μ-equation, never quantified besides.
    for var in free_vars(u, &applied) {
        if young(var) && !scheme.binds(var) {
            bind(u, &mut scheme, var);
        }
    }
    let weak: Vec<Var> = scheme.foreign().filter(|&var| is_weak(u, var)).collect();
    for var in weak {
        bind(u, &mut scheme, var);
    }
    scheme
}

/// Whether `scheme` quantifies a variable that its curried body's result
/// mentions and none of its arguments does: a result nothing the caller
/// supplies determines, which is a cast.  A scheme that is not a block has no
/// arguments, and is no cast.
#[cfg(any(test, feature = "test-util"))]
pub(crate) fn has_result_only_var(scheme: &Scheme) -> bool {
    use crate::ty::collect_vars;
    let Ty::Thunk(body) = &scheme.ty else {
        return false;
    };
    let (mut args, mut cur) = (BTreeSet::new(), &**body);
    let result = loop {
        match cur {
            CompTy::Fun(arg, rest) => {
                collect_vars(Term::Ty(arg), &mut args);
                cur = rest;
            }
            CompTy::Return(_, result) => break result,
            CompTy::Var(_) => return false,
        }
    };
    let mut results = BTreeSet::new();
    collect_vars(Term::Ty(result), &mut results);
    results.difference(&args).any(|&var| match var {
        Var::Ty(v) => matches!(scheme.tys.get(&v), Some(Role::Forall(_))),
        Var::Row(v) => matches!(scheme.rows.get(&v), Some(Role::Forall(_))),
        Var::Comp(_) | Var::Grade(_) => false,
    })
}

/// `scheme` as its unit leaves it: weak variables and residual grades resolved
/// as far as the unit fixed them, cycles snapshotted, still-free grades
/// quantified, and still-weak variables recorded.
///
/// # Panics
/// If the settled scheme still writes a variable of the unit's that is neither
/// a grade nor weak, which no successful unit leaves.
pub(crate) fn settle_weak(u: &Unifier, scheme: Arc<Scheme>) -> Closed {
    let residuals: BTreeSet<Var> = scheme
        .weak()
        .chain(scheme.foreign().filter(|var| matches!(var, Var::Grade(_))))
        .collect();
    if residuals.is_empty() {
        return closed(scheme);
    }
    // Only residuals belong to `u`; local ∀/μ names stay opaque.
    let images = (u, &residuals);
    let mut settled = Scheme {
        ty: images.ty(&scheme.ty),
        tys: local(&scheme.tys, |binding| images.ty(binding)),
        comps: local(&scheme.comps, |binding| images.comp(binding)),
        rows: local(&scheme.rows, |&never| match never {}),
        grades: scheme.grades.clone(),
    };
    for &var in &residuals {
        let image = match var {
            Var::Ty(v) => images.ty(&Ty::Var(v)),
            Var::Row(v) => Ty::Record(images.row(&Row::Var(v))),
            Var::Comp(v) => Ty::Thunk(Box::new(images.comp(&CompTy::Var(v)))),
            Var::Grade(_) => continue,
        };
        capture(u, &image, &mut settled, |_| true);
    }
    let free: Vec<Var> = settled
        .foreign()
        .filter(|&var| matches!(var, Var::Grade(_)) || is_weak(u, var))
        .collect();
    for var in free {
        bind(u, &mut settled, var);
    }
    closed(Arc::new(settled))
}

/// The binders of `vars`, weak residuals dropped, each μ-binding through `f`.
fn local<V: Ord + Copy, M: Copy, B>(
    vars: &BTreeMap<V, Role<M, B>>,
    f: impl Fn(&B) -> B,
) -> BTreeMap<V, Role<M, B>> {
    vars.iter()
        .filter(|(_, role)| !role.is_weak())
        .map(|(&v, role)| (v, role.map_mu(&f)))
        .collect()
}

fn closed(scheme: Arc<Scheme>) -> Closed {
    Closed::new(scheme).unwrap_or_else(|error| panic!("settlement left a live variable: {error}"))
}

/// A stored scheme in a later unit: its weak residuals are ids of a unifier
/// that is gone, so each becomes a fresh weak variable here, and every local
/// binder a fresh name besides, so no residual's new id captures one.
pub(crate) fn reseed_weak(u: &mut Unifier, scheme: &Closed) -> Arc<Scheme> {
    if !scheme.has_weak() {
        return Arc::clone(scheme.scheme());
    }
    let renaming = Renaming::fresh(u, scheme, true);
    let seeded = Scheme {
        ty: renaming.ty(&scheme.ty),
        tys: renamed(&scheme.tys, &renaming.tys, |binding| renaming.ty(binding)),
        comps: renamed(&scheme.comps, &renaming.comps, |binding| {
            renaming.comp(binding)
        }),
        rows: renamed(&scheme.rows, &renaming.rows, |&never| match never {}),
        grades: scheme.grades.iter().map(|v| renaming.grades[v]).collect(),
    };
    for var in seeded.weak() {
        u.mark_residual(var);
    }
    Arc::new(seeded)
}

/// `vars` with each variable through `names` and each μ-binding through `f`.
fn renamed<V: Ord + Copy + Hash, M: Copy, B>(
    vars: &BTreeMap<V, Role<M, B>>,
    names: &HashMap<V, V>,
    f: impl Fn(&B) -> B,
) -> BTreeMap<V, Role<M, B>> {
    vars.iter()
        .map(|(v, role)| (names[v], role.map_mu(&f)))
        .collect()
}

pub(crate) fn instantiate(u: &mut Unifier, scheme: &Scheme) -> Ty {
    if !scheme.is_poly() {
        return scheme.ty.clone();
    }
    let renaming = Renaming::fresh(u, scheme, false);
    // Re-binding the fresh roots is what carries the cycle across instantiation.
    for (v, role) in &scheme.tys {
        if let Role::Mu(binding) = role {
            u.bind_ty_root(renaming.tys[v], renaming.ty(binding));
        }
    }
    for (v, role) in &scheme.comps {
        if let Role::Mu(binding) = role {
            u.bind_comp_root(renaming.comps[v], renaming.comp(binding));
        }
    }
    renaming.ty(&scheme.ty)
}

/// A renaming, each variable to a fresh one of its own sort.
struct Renaming {
    tys: HashMap<TyVar, TyVar>,
    comps: HashMap<CompTyVar, CompTyVar>,
    rows: HashMap<RowVar, RowVar>,
    grades: HashMap<GradeVar, GradeVar>,
}

impl Renaming {
    /// Fresh stand-ins for `scheme`'s binders, and for its weak residuals too
    /// when `weak`.
    fn fresh(u: &mut Unifier, scheme: &Scheme, weak: bool) -> Self {
        fn each<V: Copy + Eq + Hash, M: Copy, B>(
            vars: &BTreeMap<V, Role<M, B>>,
            weak: bool,
            mut mint: impl FnMut(&Role<M, B>) -> V,
        ) -> HashMap<V, V> {
            vars.iter()
                .filter(|(_, role)| weak || !role.is_weak())
                .map(|(&v, role)| (v, mint(role)))
                .collect()
        }
        Self {
            tys: each(&scheme.tys, weak, |role| match *role {
                Role::Forall(kind) | Role::Weak(kind) => u.fresh_kinded_var(kind),
                Role::Mu(_) => u.fresh_ty_root(),
            }),
            comps: each(&scheme.comps, weak, |_| u.fresh_comp_root()),
            rows: each(&scheme.rows, weak, |role| match *role {
                Role::Forall(deep) | Role::Weak(deep) => u.fresh_deep_row_var(deep),
                Role::Mu(never) => match never {},
            }),
            grades: scheme
                .grades
                .iter()
                .map(|&v| (v, u.fresh_grade_var()))
                .collect(),
        }
    }
}

/// A simultaneous substitution, read at each variable leaf.
trait Subst {
    fn ty_var(&self, v: TyVar) -> Option<Ty>;
    fn comp_var(&self, v: CompTyVar) -> Option<CompTy>;
    fn row_var(&self, v: RowVar) -> Option<Row>;
    fn grade_var(&self, v: GradeVar) -> Option<Grade>;

    fn ty(&self, ty: &Ty) -> Ty {
        match ty {
            Ty::Var(v) => self.ty_var(*v).unwrap_or_else(|| ty.clone()),
            Ty::List(a) => Ty::list(self.ty(a)),
            Ty::Map(a) => Ty::map(self.ty(a)),
            Ty::Handle(g, a) => Ty::Handle(self.grade(*g), Box::new(self.ty(a))),
            Ty::Record(r) => Ty::Record(self.row(r)),
            Ty::Variant(r) => Ty::Variant(self.row(r)),
            Ty::Thunk(b) => Ty::Thunk(Box::new(self.comp(b))),
            // Enumerated, not `_`: cloning a new variable-carrying `Ty`
            // unsubstituted would be variable capture.
            Ty::Unit | Ty::Bytes | Ty::Bool | Ty::Int | Ty::Float | Ty::String => ty.clone(),
        }
    }

    fn row(&self, row: &Row) -> Row {
        match row {
            Row::Empty => Row::Empty,
            Row::Var(v) => self.row_var(*v).unwrap_or_else(|| row.clone()),
            Row::Extend(l, ty, rest) => {
                Row::Extend(l.clone(), Box::new(self.ty(ty)), Box::new(self.row(rest)))
            }
        }
    }

    fn comp(&self, cty: &CompTy) -> CompTy {
        match cty {
            CompTy::Var(v) => self.comp_var(*v).unwrap_or_else(|| cty.clone()),
            CompTy::Return(grade, a) => CompTy::Return(self.grade(*grade), Box::new(self.ty(a))),
            CompTy::Fun(a, b) => CompTy::Fun(Box::new(self.ty(a)), Box::new(self.comp(b))),
        }
    }

    fn grade(&self, grade: Grade) -> Grade {
        match grade {
            Grade::Var(v) => self.grade_var(v).unwrap_or(grade),
            Grade::Value | Grade::Output => grade,
        }
    }
}

impl Subst for Renaming {
    fn ty_var(&self, v: TyVar) -> Option<Ty> {
        self.tys.get(&v).copied().map(Ty::Var)
    }

    fn comp_var(&self, v: CompTyVar) -> Option<CompTy> {
        self.comps.get(&v).copied().map(CompTy::Var)
    }

    fn row_var(&self, v: RowVar) -> Option<Row> {
        self.rows.get(&v).copied().map(Row::Var)
    }

    fn grade_var(&self, v: GradeVar) -> Option<Grade> {
        self.grades.get(&v).copied().map(Grade::Var)
    }
}

/// Settlement: each listed residual's image in the unit's unifier.
impl Subst for (&Unifier, &BTreeSet<Var>) {
    fn ty_var(&self, v: TyVar) -> Option<Ty> {
        self.1
            .contains(&Var::Ty(v))
            .then(|| self.0.apply_ty(&Ty::Var(v)))
    }

    fn comp_var(&self, v: CompTyVar) -> Option<CompTy> {
        self.1
            .contains(&Var::Comp(v))
            .then(|| self.0.apply_comp_ty(&CompTy::Var(v)))
    }

    fn row_var(&self, v: RowVar) -> Option<Row> {
        self.1
            .contains(&Var::Row(v))
            .then(|| self.0.apply_row(&Row::Var(v)))
    }

    fn grade_var(&self, v: GradeVar) -> Option<Grade> {
        self.1
            .contains(&Var::Grade(v))
            .then(|| self.0.resolve_grade(Grade::Var(v)))
    }
}

#[cfg(test)]
mod tests {
    use super::super::unify::WeakSource;
    use super::*;
    use crate::ty::{Kind, Label};

    fn forall(scheme: &Scheme) -> Vec<Var> {
        scheme.quantified().collect()
    }

    fn weak(scheme: &Scheme) -> Vec<Var> {
        scheme.weak().collect()
    }

    fn mus(scheme: &Scheme) -> usize {
        scheme.tys.values().filter_map(Role::mu).count()
            + scheme.comps.values().filter_map(Role::mu).count()
    }

    fn generalized(u: &mut Unifier, ty: &Ty) -> Scheme {
        u.exit();
        generalize(u, ty)
    }

    fn arrow(param: Ty, result: Ty) -> Ty {
        Ty::Thunk(Box::new(CompTy::Fun(
            Box::new(param),
            Box::new(CompTy::pure(result)),
        )))
    }

    fn unit_block(grade: Grade) -> Ty {
        Ty::Thunk(Box::new(CompTy::Return(grade, Box::new(Ty::Unit))))
    }

    fn recursive_row_parts(u: &Unifier, ty: &Ty) -> (HashMap<String, Ty>, Row) {
        let Ty::Record(mut row) = u.resolve_ty(ty) else {
            panic!("record")
        };
        let mut fields = HashMap::new();
        loop {
            row = u.resolve_row(&row);
            match row {
                Row::Extend(label, payload, rest) => {
                    assert!(fields.insert(label.name().to_owned(), *payload).is_none());
                    row = *rest;
                }
                tail => return (fields, tail),
            }
        }
    }

    #[test]
    fn recursive_row_payload_schemes_close_and_instantiate_independently_with_sharing() {
        let mut u = Unifier::new();
        u.enter();
        let rho = u.fresh_row_var();
        let residual = u.fresh_row_var();
        let item = u.fresh_kinded(Kind::COMPARABLE);
        let inner = crate::ty::open_record(&[("item", item.clone()), ("mirror", item)], rho);
        let Ty::Record(proposed) = crate::ty::open_record(&[("next", inner.clone())], residual)
        else {
            unreachable!()
        };
        u.unify_row(&Row::Var(rho), &proposed).unwrap();
        let scheme = Arc::new(generalized(&mut u, &inner));
        let scheme = settle_weak(&u, scheme);
        let sorts = forall(&scheme);
        assert!(matches!(sorts[..], [Var::Ty(_), Var::Row(_)]), "{sorts:?}");
        assert_ne!(mus(&scheme), 0);
        assert!(scheme.to_string().contains('μ'));
        let bytes = postcard::to_allocvec(&scheme).unwrap();
        let stored: Closed = postcard::from_bytes(&bytes).unwrap();
        let mut next = Unifier::new();
        next.enter();
        let first = instantiate(&mut next, &stored);
        let second = instantiate(&mut next, &stored);
        for (instance, rigid) in [(&first, Ty::Int), (&second, Ty::String)] {
            let (fields, tail) = recursive_row_parts(&next, instance);
            assert_eq!(fields.len(), 3);
            next.unify_ty(&fields["item"], &rigid).unwrap();
            assert_eq!(next.apply_ty(&fields["mirror"]), rigid);
            assert!(matches!(tail, Row::Var(_)));
            let Ty::Record(closed) = crate::ty::closed_record(&[("extra", rigid.clone())]) else {
                unreachable!()
            };
            next.unify_row(&tail, &closed).unwrap();
            let mut current = instance.clone();
            for _ in 0..4 {
                let (fields, rest) = recursive_row_parts(&next, &current);
                assert_eq!(rest, Row::Empty);
                assert_eq!(fields.len(), 4);
                for name in ["item", "mirror", "extra"] {
                    assert_eq!(next.apply_ty(&fields[name]), rigid);
                }
                current = fields["next"].clone();
            }
        }
        assert!(next.unify_ty(&first, &second).is_err());
    }

    #[test]
    fn settling_a_weak_row_image_captures_new_payload_cycles_without_young_aliases() {
        let mut u = Unifier::new();
        u.enter();
        let rho = u.fresh_row_var();
        let ty = Ty::Record(Row::Var(rho));
        u.mark_weak(&ty, &WeakSource::Boundary("from-json".into()));
        let harvested = Arc::new(generalized(&mut u, &ty));
        assert_eq!(
            *harvested,
            Scheme {
                rows: [(rho, Role::Weak(false))].into(),
                ..Scheme::mono(ty)
            }
        );
        u.enter();
        let inner = crate::ty::open_record(&[("item", Ty::Int)], rho);
        let Ty::Record(proposed) = crate::ty::closed_record(&[("next", inner)]) else {
            unreachable!()
        };
        u.unify_row(&Row::Var(rho), &proposed).unwrap();
        u.exit();
        let settled = settle_weak(&u, harvested);
        assert!(!settled.has_weak());
        assert_eq!(forall(&settled), []);
        assert_ne!(mus(&settled), 0);
        assert!(settled.to_string().contains('μ'));
        let mut next = Unifier::new();
        let instance = instantiate(&mut next, &settled);
        let (fields, tail) = recursive_row_parts(&next, &instance);
        assert_eq!(tail, Row::Empty);
        assert_eq!(fields.len(), 1);
        let mut current = fields["next"].clone();
        for _ in 0..4 {
            let (fields, tail) = recursive_row_parts(&next, &current);
            assert_eq!(tail, Row::Empty);
            assert_eq!(fields.len(), 2);
            assert_eq!(next.apply_ty(&fields["item"]), Ty::Int);
            current = fields["next"].clone();
        }
    }

    #[test]
    fn older_roots_lower_later_structure_in_every_sort() {
        for kind in 0..3 {
            let mut u = Unifier::new();
            u.enter();
            let seed = match kind {
                0 => u.fresh_ty(),
                1 => Ty::Thunk(Box::new(u.fresh_comp_ty())),
                _ => Ty::Record(u.fresh_row()),
            };
            u.enter();
            let inner = u.fresh_tyvar();
            let structure = match kind {
                0 => Ty::list(Ty::Var(inner)),
                1 => Ty::Thunk(Box::new(CompTy::pure(Ty::Var(inner)))),
                _ => Ty::Record(Row::Extend(
                    Label::Field("a".into()),
                    Box::new(Ty::Var(inner)),
                    Box::new(Row::Empty),
                )),
            };
            u.unify_ty(&seed, &structure).unwrap();
            u.exit();
            let next = generalize(&u, &Ty::Var(inner));
            assert_eq!(forall(&next), [], "residual kind {kind}");
            assert!(!next.has_weak(), "rank one is old, not weak");
        }
    }

    #[test]
    fn a_weak_variable_survives_generalisation() {
        let mut u = Unifier::new();
        u.enter();
        let (bound, weak) = (u.fresh_tyvar(), u.fresh_tyvar());
        u.mark_weak(&Ty::Var(weak), &WeakSource::Boundary("from-json".into()));
        let ty = arrow(Ty::Var(bound), Ty::Var(weak));
        let scheme = generalized(&mut u, &ty);
        assert_eq!(
            scheme,
            Scheme {
                tys: [
                    (bound, Role::Forall(Kind::ANY)),
                    (weak, Role::Weak(Kind::ANY))
                ]
                .into(),
                ..Scheme::mono(ty)
            }
        );
    }

    #[test]
    fn weak_rows_and_computations_survive_generalisation() {
        let mut u = Unifier::new();
        u.enter();
        let (row, comp) = (u.fresh_row_var(), u.fresh_comp_ty());
        let ty = Ty::list(Ty::Record(Row::Var(row)));
        let body = Ty::Thunk(Box::new(comp.clone()));
        u.mark_weak(&ty, &WeakSource::Boundary("from-json".into()));
        u.mark_weak(&body, &WeakSource::Boundary("from-json".into()));
        let CompTy::Var(comp) = comp else {
            unreachable!("fresh_comp_ty yields a Var")
        };
        let ty = arrow(ty, body);
        let scheme = generalized(&mut u, &ty);
        assert_eq!(
            scheme,
            Scheme {
                comps: [(comp, Role::Weak(()))].into(),
                rows: [(row, Role::Weak(false))].into(),
                ..Scheme::mono(ty)
            }
        );
    }

    #[test]
    fn a_variable_united_with_a_weak_one_is_weak_whichever_side_it_is_on() {
        for weak_first in [true, false] {
            let mut u = Unifier::new();
            u.enter();
            let (weak, other) = (u.fresh_tyvar(), u.fresh_tyvar());
            u.mark_weak(&Ty::Var(weak), &WeakSource::Boundary("from-json".into()));
            let (a, b) = (Ty::Var(weak), Ty::Var(other));
            let result = if weak_first {
                u.unify_ty(&a, &b)
            } else {
                u.unify_ty(&b, &a)
            };
            result.expect("two variables unite");
            assert!(u.is_weak_ty(other) && u.is_weak_ty(weak));
            let scheme = generalized(&mut u, &arrow(b, Ty::Unit));
            assert_eq!(forall(&scheme), []);
        }
    }

    #[test]
    fn binding_a_weak_variable_to_a_structure_makes_what_it_mentions_weak() {
        let mut u = Unifier::new();
        u.enter();
        let (weak, inner) = (u.fresh_tyvar(), u.fresh_tyvar());
        u.mark_weak(&Ty::Var(weak), &WeakSource::Boundary("from-json".into()));
        u.unify_ty(&Ty::Var(weak), &Ty::list(Ty::Var(inner)))
            .expect("a free variable binds");
        let scheme = generalized(&mut u, &arrow(Ty::Var(weak), Ty::Unit));
        assert_eq!(forall(&scheme), []);
        assert!(u.is_weak_ty(inner));
        assert_eq!(scheme.tys, [(weak, Role::Weak(Kind::ANY))].into());
        let settled = settle_weak(&u, Arc::new(scheme));
        assert_eq!(settled.tys, [(inner, Role::Weak(Kind::ANY))].into());
    }

    /// Weak rows and computations inherit too, and stay themselves in a scheme
    /// after they are fixed.
    #[test]
    fn a_row_unified_with_a_fixed_weak_row_is_weak_and_kept() {
        let mut u = Unifier::new();
        u.enter();
        let (weak, other) = (u.fresh_row_var(), u.fresh_row_var());
        u.mark_weak(
            &Ty::Record(Row::Var(weak)),
            &WeakSource::Boundary("from-json".into()),
        );
        let spine = Row::Extend(
            Label::Field("a".into()),
            Box::new(Ty::Int),
            Box::new(Row::Empty),
        );
        u.unify_row(&Row::Var(weak), &spine)
            .expect("a free row binds");
        u.unify_row(&Row::Var(other), &Row::Var(weak))
            .expect("a free row binds the spine");
        assert!(u.is_weak_row(other));
        let kept = u.apply_ty_keeping_weak(&Ty::Record(Row::Var(weak)));
        assert_eq!(kept, Ty::Record(Row::Var(weak)));
        assert_ne!(u.apply_ty(&Ty::Record(Row::Var(weak))), kept);
    }

    #[test]
    fn a_computation_unified_with_a_fixed_weak_one_is_weak() {
        let mut u = Unifier::new();
        u.enter();
        let (weak, other) = (u.fresh_comp_ty(), u.fresh_comp_ty());
        u.mark_weak(
            &Ty::Thunk(Box::new(weak.clone())),
            &WeakSource::Boundary("from-json".into()),
        );
        u.unify_comp_ty(&weak, &CompTy::pure(Ty::Int))
            .expect("a free computation binds");
        u.unify_comp_ty(&other, &weak)
            .expect("a free computation binds");
        let CompTy::Var(other) = other else {
            unreachable!("fresh_comp_ty yields a Var")
        };
        assert!(u.is_weak_comp(other));
    }

    /// A stored scheme carries ids of a unifier that is gone.
    #[test]
    fn a_residual_round_trips_and_reseeds_fresh_and_weak() {
        let mut first = Unifier::new();
        first.enter();
        for _ in 0..1000 {
            first.fresh_tyvar();
        }
        let (bound, weak) = (first.fresh_tyvar(), first.fresh_tyvar());
        first.mark_weak(&Ty::Var(weak), &WeakSource::Boundary("from-json".into()));
        let scheme = Arc::new(generalized(
            &mut first,
            &arrow(Ty::Var(bound), Ty::Var(weak)),
        ));
        let scheme = settle_weak(&first, scheme);
        let bytes = postcard::to_allocvec(&scheme).expect("a scheme serialises");
        let stored: Closed = postcard::from_bytes(&bytes).expect("and reads back");
        assert_eq!(stored, scheme);

        let mut next = Unifier::new();
        let seeded = reseed_weak(&mut next, &stored);
        let [Var::Ty(fresh)] = super::tests::weak(&seeded)[..] else {
            panic!("one residual in, one residual out: {seeded:?}");
        };
        assert_ne!(fresh, weak);
        assert!(next.is_weak_ty(fresh));
        let [Var::Ty(bound)] = forall(&seeded)[..] else {
            panic!("one quantified variable")
        };
        assert_eq!(seeded.tys[&bound], Role::Forall(Kind::ANY));
        assert_ne!(bound, fresh);
        assert_eq!(seeded.ty, arrow(Ty::Var(bound), Ty::Var(fresh)));
    }

    #[test]
    fn a_quantified_variable_keeps_its_kind_across_instantiation() {
        let mut u = Unifier::new();
        u.enter();
        let v = u.fresh_kinded_var(Kind::NUMBER);
        let scheme = generalized(&mut u, &arrow(Ty::Var(v), Ty::Var(v)));
        assert_eq!(scheme.tys, [(v, Role::Forall(Kind::NUMBER))].into());
        assert_eq!(scheme.to_string(), "∀α:number. α → Returns α");
        let Ty::Thunk(body) = instantiate(&mut u, &scheme) else {
            panic!("a function scheme instantiates to a thunk")
        };
        let CompTy::Fun(param, _) = *body else {
            panic!("a function")
        };
        let Ty::Var(fresh) = *param else {
            panic!("a fresh variable")
        };
        assert_ne!(fresh, v);
        assert_eq!(u.kinded(fresh).kind, Kind::NUMBER);
    }

    #[test]
    fn a_deep_row_is_quantified_deep_and_instantiates_deep() {
        let mut u = Unifier::new();
        u.enter();
        let row = u.fresh_deep_row_var(true);
        let scheme = generalized(&mut u, &arrow(Ty::Record(Row::Var(row)), Ty::Unit));
        assert_eq!(scheme.rows, [(row, Role::Forall(true))].into());
        let Ty::Thunk(body) = instantiate(&mut u, &scheme) else {
            panic!("a thunk")
        };
        let CompTy::Fun(param, _) = *body else {
            panic!("a function")
        };
        let Ty::Record(Row::Var(fresh)) = *param else {
            panic!("a record on a fresh row")
        };
        assert!(u.is_deep_row(fresh));
    }

    #[test]
    fn a_kind_round_trips_through_postcard_in_a_scheme_and_in_its_residuals() {
        let mut first = Unifier::new();
        first.enter();
        let (bound, weak) = (
            first.fresh_kinded_var(Kind::SIZED),
            first.fresh_kinded_var(Kind::COMPARABLE),
        );
        first.mark_weak(&Ty::Var(weak), &WeakSource::Boundary("from-json".into()));
        let scheme = Arc::new(generalized(
            &mut first,
            &arrow(Ty::Var(bound), Ty::Var(weak)),
        ));
        let scheme = settle_weak(&first, scheme);
        let bytes = postcard::to_allocvec(&scheme).expect("a scheme serialises");
        let stored: Closed = postcard::from_bytes(&bytes).expect("and reads back");
        assert_eq!(stored, scheme);
        assert_eq!(stored.tys[&bound], Role::Forall(Kind::SIZED));

        let mut next = Unifier::new();
        let seeded = reseed_weak(&mut next, &stored);
        let [Var::Ty(fresh)] = super::tests::weak(&seeded)[..] else {
            panic!("one residual in, one out: {seeded:?}");
        };
        assert_eq!(seeded.tys[&fresh], Role::Weak(Kind::COMPARABLE));
        assert_eq!(next.kinded(fresh).kind, Kind::COMPARABLE);
        assert!(next.is_weak_ty(fresh));
    }

    /// The seed bequest travels as JSON, whose map keys are strings.
    #[test]
    fn a_scheme_of_every_sort_and_role_round_trips_through_json() {
        let (bound, weak, root) = (TyVar(0), TyVar(1), TyVar(2));
        let (comp, row, grade) = (CompTyVar(0), RowVar(0), GradeVar(0));
        let ty = crate::ty::open_record(
            &[
                ("bound", Ty::Var(bound)),
                ("weak", Ty::Var(weak)),
                ("root", Ty::Var(root)),
                ("comp", Ty::Thunk(Box::new(CompTy::Var(comp)))),
                ("grade", unit_block(Grade::Var(grade))),
            ],
            row,
        );
        let scheme = Closed::new(Arc::new(Scheme {
            tys: [
                (bound, Role::Forall(Kind::SIZED)),
                (weak, Role::Weak(Kind::ANY)),
                (root, Role::Mu(Ty::list(Ty::Var(root)))),
            ]
            .into(),
            comps: [(comp, Role::Forall(()))].into(),
            rows: [(row, Role::Weak(true))].into(),
            grades: [grade].into(),
            ..Scheme::mono(ty)
        }))
        .unwrap();
        let json = serde_json::to_string(&scheme).expect("a scheme serialises");
        let stored: Closed = serde_json::from_str(&json).expect("and reads back");
        assert_eq!(stored, scheme);
    }

    #[test]
    fn reseeding_cannot_capture_a_quantifier_or_a_recursive_binder() {
        for recursive in [false, true] {
            let mut first = Unifier::new();
            first.enter();
            // Ids are reseeded in order, so the residual's lands on `bound`'s.
            let _ = first.fresh_tyvar();
            let (bound, weak) = (first.fresh_tyvar(), first.fresh_tyvar());
            first.mark_weak(&Ty::Var(weak), &WeakSource::Boundary("from-json".into()));
            if recursive {
                first.bind_ty_root(bound, Ty::list(Ty::Var(bound)));
            }
            let scheme = Arc::new(generalized(
                &mut first,
                &arrow(Ty::Var(bound), Ty::Var(weak)),
            ));
            let scheme = settle_weak(&first, scheme);
            let mut next = Unifier::new();
            let seeded = reseed_weak(&mut next, &scheme);
            let [Var::Ty(fresh_weak)] = super::tests::weak(&seeded)[..] else {
                panic!("one residual: {seeded:?}")
            };
            assert_eq!(fresh_weak, bound, "force the old namespace collision");
            let instance = instantiate(&mut next, &seeded);
            let Ty::Thunk(body) = instance else {
                panic!("a thunk")
            };
            let CompTy::Fun(param, rest) = *body else {
                panic!("a function")
            };
            let CompTy::Return(_, result) = *rest else {
                panic!("a result")
            };
            assert_eq!(*result, Ty::Var(fresh_weak));
            assert_ne!(*param, *result);
            next.unify_ty(&result, &Ty::Int)
                .expect("the weak result fixes independently");
            assert_ne!(next.apply_ty(&param), Ty::Int);
        }
    }

    #[test]
    fn reseeding_cannot_capture_row_or_computation_quantifiers() {
        let mut first = Unifier::new();
        first.enter();
        let (bound_row, weak_row) = (first.fresh_row_var(), first.fresh_row_var());
        let (bound_comp, weak_comp) = (first.fresh_comp_ty(), first.fresh_comp_ty());
        let ty = arrow(
            arrow(
                Ty::Record(Row::Var(bound_row)),
                Ty::Thunk(Box::new(bound_comp)),
            ),
            arrow(
                Ty::Record(Row::Var(weak_row)),
                Ty::Thunk(Box::new(weak_comp)),
            ),
        );
        let Ty::Thunk(body) = &ty else { unreachable!() };
        let CompTy::Fun(_, rest) = &**body else {
            unreachable!()
        };
        let CompTy::Return(_, result) = &**rest else {
            unreachable!()
        };
        first.mark_weak(result, &WeakSource::Boundary("from-json".into()));
        let scheme = Arc::new(generalized(&mut first, &ty));
        let scheme = settle_weak(&first, scheme);
        let mut next = Unifier::new();
        let seeded = reseed_weak(&mut next, &scheme);
        let (bound, residual) = (forall(&seeded), super::tests::weak(&seeded));
        assert!(
            matches!(bound[..], [Var::Comp(_), Var::Row(_)]),
            "{bound:?}"
        );
        assert!(
            matches!(residual[..], [Var::Comp(_), Var::Row(_)]),
            "{residual:?}"
        );
        let instance = instantiate(&mut next, &seeded);
        let fvs = free_vars(&next, &instance);
        assert_eq!(
            fvs.iter().filter(|var| matches!(var, Var::Row(_))).count(),
            2
        );
        assert_eq!(
            fvs.iter().filter(|var| matches!(var, Var::Comp(_))).count(),
            2
        );
        assert!(seeded.is_closed());
    }

    #[test]
    fn settling_quantifies_grades_exposed_by_weak_images() {
        let mut u = Unifier::new();
        u.enter();
        let weak = u.fresh_ty();
        u.mark_weak(&weak, &WeakSource::Boundary("from-json".into()));
        let scheme = Arc::new(generalized(&mut u, &weak));
        let grade = u.fresh_grade_var();
        let block = Ty::Thunk(Box::new(CompTy::Return(
            Grade::Var(grade),
            Box::new(Ty::Unit),
        )));
        u.unify_ty(&weak, &block).unwrap();
        let settled = settle_weak(&u, scheme);
        assert_eq!(settled.ty, block);
        assert_eq!(settled.grades, [grade].into());
        assert!(!settled.has_weak());
    }

    #[test]
    fn settled_grades_are_opaque_in_the_next_session() {
        let mut first = Unifier::new();
        let weak = first.fresh_ty();
        first.mark_weak(&weak, &WeakSource::Host("sv".into()));
        first.enter();
        let grade = first.fresh_grade_var();
        let block = unit_block(Grade::Var(grade));
        first.unify_ty(&weak, &block).unwrap();
        first.exit();
        let scheme = generalize(&first, &weak);
        assert!(scheme.grades.is_empty());
        let settled = settle_weak(&first, Arc::new(scheme));
        assert_eq!(settled.grades, [grade].into());
        assert!(!settled.has_weak());
        let bytes = postcard::to_allocvec(&settled).unwrap();
        let stored: Closed = postcard::from_bytes(&bytes).unwrap();

        let mut next = Unifier::new();
        let seeded = reseed_weak(&mut next, &stored);
        next.enter();
        let fresh = next.fresh_grade_var();
        assert_eq!(fresh, grade, "force the old namespace collision");
        let block = unit_block(Grade::Var(fresh));
        next.exit();
        let scheme = generalize(&next, &block);
        assert_eq!(scheme.grades, [fresh].into());
        assert!(scheme.is_closed());
        assert!(seeded.is_closed());
    }

    #[test]
    fn settling_closes_grade_only_residuals_without_weak_anchors() {
        let mut u = Unifier::new();
        let grade = u.fresh_grade_var();
        u.enter();
        let block = unit_block(Grade::Var(grade));
        u.exit();
        let scheme = generalize(&u, &block);
        assert!(!scheme.has_weak());
        assert!(scheme.grades.is_empty());
        assert!(!scheme.is_closed());

        let settled = settle_weak(&u, Arc::new(scheme));
        assert_eq!(settled.ty, block);
        assert_eq!(settled.grades, [grade].into());
        assert!(!settled.has_weak());
    }

    #[test]
    fn settling_resolves_grades_fixed_after_generalization() {
        for weak_data in [false, true] {
            for fixed in [Grade::Value, Grade::Output] {
                let mut u = Unifier::new();
                let grade = u.fresh_grade_var();
                u.enter();
                let block = unit_block(Grade::Var(grade));
                let data = if weak_data {
                    let data = u.fresh_ty();
                    u.mark_weak(&data, &WeakSource::Host("sv_data".into()));
                    data
                } else {
                    Ty::Unit
                };
                u.exit();
                let scheme = generalize(&u, &arrow(block, data.clone()));
                assert!(scheme.grades.is_empty());
                u.bind_grade(grade, fixed);

                let settled = settle_weak(&u, Arc::new(scheme));
                assert_eq!(settled.ty, arrow(unit_block(fixed), data));
                assert!(settled.grades.is_empty());
                assert_eq!(settled.has_weak(), weak_data);
            }
        }
    }

    #[test]
    fn settling_quantifies_only_resolved_grade_roots() {
        let mut u = Unifier::new();
        let grade = u.fresh_grade_var();
        u.enter();
        let root = u.fresh_grade_var();
        let block = unit_block(Grade::Var(grade));
        u.exit();
        let scheme = generalize(&u, &block);
        u.unify_comp_ty(
            &CompTy::Return(Grade::Var(grade), Box::new(Ty::Unit)),
            &CompTy::Return(Grade::Var(root), Box::new(Ty::Unit)),
        )
        .unwrap();
        assert_eq!(u.resolve_grade(Grade::Var(grade)), Grade::Var(root));

        let settled = settle_weak(&u, Arc::new(scheme));
        assert_eq!(settled.ty, unit_block(Grade::Var(root)));
        assert_eq!(settled.grades, [root].into());
    }

    #[test]
    fn settlement_keeps_quantified_grades_opaque_while_resolving_residuals() {
        let mut u = Unifier::new();
        let residual = u.fresh_grade_var();
        u.enter();
        let local = u.fresh_grade_var();
        let weak = u.fresh_ty();
        u.mark_weak(&weak, &WeakSource::Host("sv_data".into()));
        let ty = arrow(
            unit_block(Grade::Var(local)),
            arrow(unit_block(Grade::Var(residual)), weak.clone()),
        );
        u.exit();
        let scheme = generalize(&u, &ty);
        assert_eq!(scheme.grades, [local].into());
        u.bind_grade(local, Grade::Value);
        u.bind_grade(residual, Grade::Output);

        let settled = settle_weak(&u, Arc::new(scheme));
        assert_eq!(
            settled.ty,
            arrow(
                unit_block(Grade::Var(local)),
                arrow(unit_block(Grade::Output), weak),
            )
        );
        assert_eq!(settled.grades, [local].into());
        assert!(settled.has_weak());
    }

    #[test]
    fn settling_refreshes_weak_references_inside_recursive_snapshots() {
        let mut u = Unifier::new();
        u.enter();
        let (root, weak) = (u.fresh_tyvar(), u.fresh_tyvar());
        u.mark_weak(&Ty::Var(weak), &WeakSource::Boundary("from-json".into()));
        u.bind_ty_root(root, Ty::list(arrow(Ty::Var(weak), Ty::Var(root))));
        let scheme = Arc::new(generalized(&mut u, &Ty::Var(root)));
        u.unify_ty(&Ty::Var(weak), &Ty::Int).unwrap();
        let settled = settle_weak(&u, scheme);
        assert!(!settled.has_weak());
        assert!(!settled.syntactic_vars().contains(&Var::Ty(weak)));
        let bytes = postcard::to_allocvec(&settled).unwrap();
        let stored: Closed = postcard::from_bytes(&bytes).unwrap();
        let mut next = Unifier::new();
        let instance = instantiate(&mut next, &stored);
        assert!(next.weak_source_in_ty(&instance).is_none());
        assert_eq!(next.cyclic_roots_in_ty(&instance).1.len(), 1);
    }

    #[test]
    fn settling_an_imported_computation_cycle_walks_its_equation() {
        let mut first = Unifier::new();
        first.enter();
        let weak = first.fresh_tyvar();
        let root = first.fresh_comp_root();
        first.mark_weak(&Ty::Var(weak), &WeakSource::Boundary("from-json".into()));
        let body = CompTy::pure(Ty::list(arrow(
            Ty::Var(weak),
            Ty::Thunk(Box::new(CompTy::Var(root))),
        )));
        let scheme = Closed::new(Arc::new(Scheme {
            tys: [(weak, Role::Weak(Kind::ANY))].into(),
            comps: [(root, Role::Mu(body))].into(),
            ..Scheme::mono(Ty::Thunk(Box::new(CompTy::Var(root))))
        }))
        .unwrap();
        let mut next = Unifier::new();
        let seeded = reseed_weak(&mut next, &scheme);
        let [Var::Ty(weak)] = super::tests::weak(&seeded)[..] else {
            panic!("one residual: {seeded:?}")
        };
        next.unify_ty(&Ty::Var(weak), &Ty::list(Ty::Var(weak)))
            .unwrap();
        assert!(
            next.weak_source_in_ty(&seeded.ty).is_none(),
            "the local μ-name is opaque"
        );
        let settled = settle_weak(&next, seeded);
        assert!(!settled.has_weak());
        assert_eq!(
            mus(&settled),
            2,
            "capture the new weak cycle inside the equation"
        );
        let mut later = Unifier::new();
        let instance = instantiate(&mut later, &settled);
        let (comps, tys) = later.cyclic_roots_in_ty(&instance);
        assert_eq!((comps.len(), tys.len()), (1, 1));
    }

    #[test]
    fn closure_checks_snapshot_bodies_without_following_live_slots() {
        let mut u = Unifier::new();
        u.enter();
        let root = u.fresh_tyvar();
        let dangling = u.fresh_tyvar();
        u.unify_ty(&Ty::Var(dangling), &Ty::Int).unwrap();
        let mu = |binding| Scheme {
            tys: [(root, Role::Mu(binding))].into(),
            ..Scheme::mono(Ty::Var(root))
        };
        assert!(!mu(Ty::list(Ty::Var(dangling))).is_closed());
        let cyclic = mu(Ty::list(Ty::Var(root)));
        assert!(cyclic.is_closed());
        let open = Role::Mu(CompTy::pure(Ty::Record(Row::Var(RowVar(0)))));
        assert!(
            !Scheme {
                comps: [(CompTyVar(0), open)].into(),
                ..cyclic
            }
            .is_closed()
        );
    }

    #[test]
    fn settlement_does_not_resolve_local_binders_in_the_live_unifier() {
        let mut u = Unifier::new();
        u.enter();
        let (local, weak) = (u.fresh_tyvar(), u.fresh_tyvar());
        u.mark_weak(&Ty::Var(weak), &WeakSource::Boundary("from-json".into()));
        let scheme = Arc::new(generalized(&mut u, &arrow(Ty::Var(local), Ty::Var(weak))));
        u.bind_ty_root(local, Ty::list(Ty::Var(local)));
        u.unify_ty(&Ty::Var(weak), &Ty::Int).unwrap();
        let settled = settle_weak(&u, scheme);
        assert_eq!(settled.ty, arrow(Ty::Var(local), Ty::Int));
        assert_eq!(mus(&settled), 0);
        assert!(!settled.has_weak());
    }

    #[test]
    fn already_fixed_weak_anchors_are_still_settled() {
        let mut u = Unifier::new();
        u.enter();
        let weak = u.fresh_tyvar();
        u.mark_weak(&Ty::Var(weak), &WeakSource::Boundary("from-json".into()));
        u.unify_ty(&Ty::Var(weak), &Ty::Int).unwrap();
        let scheme = Arc::new(generalized(&mut u, &Ty::Var(weak)));
        let settled = settle_weak(&u, scheme);
        assert_eq!(settled.ty, Ty::Int);
        assert!(!settled.has_weak());
    }

    #[test]
    fn a_unit_settles_what_it_fixed_before_storing() {
        let mut u = Unifier::new();
        u.enter();
        let (fixed, open) = (u.fresh_tyvar(), u.fresh_tyvar());
        let ty = arrow(Ty::Var(fixed), Ty::Var(open));
        u.mark_weak(&ty, &WeakSource::Boundary("from-json".into()));
        let scheme = Arc::new(generalized(&mut u, &ty));
        u.unify_ty(&Ty::Var(fixed), &Ty::Int)
            .expect("a free variable binds");
        let settled = settle_weak(&u, scheme);
        assert_eq!(settled.ty, arrow(Ty::Int, Ty::Var(open)));
        assert_eq!(settled.tys, [(open, Role::Weak(Kind::ANY))].into());
    }
}
