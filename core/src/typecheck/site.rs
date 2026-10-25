//! Building a [`Site`]: the checker freezes the type it solved at a boundary,
//! and re-instantiates it for the module door's scheme check.
//!
//! The graph keeps two things a `Ty` loses once the unifier has resolved it:
//! the span that imposed each structure, and the identity of each free
//! variable, which is what lets two decodes the program treats as one
//! `comparable` type agree (a session's [`crate::ty::Fixings`], under the
//! site's unit).  The runtime walks it in `shell::runtime::admit`.

use super::unify::{Unifier, WeakSource};
use crate::ty::{
    CompTy, CompTyVar, Fields, Grade, GradeShape, Id, Label, Node, Row, RowVar, Scheme, Shape,
    Site, Tail, Ty, TyVar, UnitId,
};
use std::collections::HashMap;
use std::sync::Arc;

impl GradeShape {
    fn of(grade: Grade) -> Self {
        match grade {
            Grade::Value => Self::Value,
            Grade::Output => Self::Output,
            Grade::Var(v) => Self::Free(v.0),
        }
    }

    fn instantiate(self, u: &mut Unifier, grades: &mut HashMap<u32, Grade>) -> Grade {
        match self {
            Self::Value => Grade::Value,
            Self::Output => Grade::Output,
            Self::Free(var) => *grades.entry(var).or_insert_with(|| u.fresh_grade()),
        }
    }
}

impl Site {
    /// Freeze `ty` as `u` has solved it.
    pub(crate) fn snapshot(u: &Unifier, ty: &Ty, unit: UnitId) -> Self {
        let mut build = Build {
            u,
            nodes: Vec::new(),
            tys: HashMap::new(),
            comps: HashMap::new(),
        };
        let root = build.ty(ty);
        Self {
            nodes: build.nodes.into(),
            root,
            unit,
        }
    }

    /// Whether both sites belong to one unit.
    #[cfg(test)]
    pub(crate) fn same_unit(&self, other: &Self) -> bool {
        self.unit == other.unit
    }

    /// Freshen each node, row tail and grade once, preserving sharing and cycles.
    pub(super) fn instantiate(&self, u: &mut Unifier) -> Ty {
        let mut tys: HashMap<Id, TyVar> = HashMap::new();
        let mut comps: HashMap<Id, CompTyVar> = HashMap::new();
        let mut rows = HashMap::new();
        let mut grades = HashMap::new();
        for (id, node) in self.nodes.iter().enumerate() {
            match node.shape {
                Shape::Return(..) | Shape::Fun(..) | Shape::FreeComp { .. } => {
                    comps.insert(id, u.fresh_comp_root());
                }
                _ => {
                    tys.insert(id, u.fresh_ty_root());
                }
            }
        }
        let ty = |id: Id| Ty::Var(tys[&id]);
        let comp = |id: Id| CompTy::Var(comps[&id]);
        for (id, node) in self.nodes.iter().enumerate() {
            match &node.shape {
                Shape::Free { kind, weak, .. } => {
                    let free = Ty::Var(u.fresh_kinded_var(*kind));
                    if *weak {
                        u.mark_weak(&free, &WeakSource::Residual);
                    }
                    u.bind_ty_root(tys[&id], free);
                }
                Shape::FreeComp { weak } => {
                    let free = u.fresh_comp_ty();
                    if *weak {
                        u.mark_weak(&Ty::Thunk(Box::new(free.clone())), &WeakSource::Residual);
                    }
                    u.bind_comp_root(comps[&id], free);
                }
                Shape::Return(grade, inner) => {
                    let grade = grade.instantiate(u, &mut grades);
                    u.bind_comp_root(comps[&id], CompTy::Return(grade, Box::new(ty(*inner))));
                }
                Shape::Fun(arg, body) => {
                    u.bind_comp_root(
                        comps[&id],
                        CompTy::Fun(Box::new(ty(*arg)), Box::new(comp(*body))),
                    );
                }
                structure => {
                    let built = match structure {
                        Shape::Unit => Ty::Unit,
                        Shape::Bytes => Ty::Bytes,
                        Shape::Bool => Ty::Bool,
                        Shape::Int => Ty::Int,
                        Shape::Float => Ty::Float,
                        Shape::String => Ty::String,
                        Shape::List(e) => Ty::list(ty(*e)),
                        Shape::Map(e) => Ty::map(ty(*e)),
                        Shape::Handle(grade, e) => {
                            Ty::Handle(grade.instantiate(u, &mut grades), Box::new(ty(*e)))
                        }
                        Shape::Thunk(c) => Ty::Thunk(Box::new(comp(*c))),
                        Shape::Record(fields) => {
                            Ty::Record(row_of(u, fields, &ty, Label::Field, &mut rows))
                        }
                        Shape::Variant(fields) => {
                            Ty::Variant(row_of(u, fields, &ty, Label::Case, &mut rows))
                        }
                        Shape::Free { .. }
                        | Shape::Return(..)
                        | Shape::Fun(..)
                        | Shape::FreeComp { .. } => unreachable!("bound above"),
                    };
                    u.bind_ty_root(tys[&id], built);
                }
            }
        }
        if let Some(root) = comps.get(&self.root) {
            Ty::Thunk(Box::new(CompTy::Var(*root)))
        } else {
            ty(self.root)
        }
    }

    /// Close a contract in its own namespace before it enters another checker.
    pub(crate) fn scheme(&self) -> Arc<Scheme> {
        let mut u = Unifier::new();
        let ty = self.instantiate(&mut u);
        Arc::new(super::generalize::generalize(
            &u,
            &super::env::TyEnv::new(),
            &super::generalize::FreeVars::new(),
            &ty,
        ))
    }

    /// The module door's view of the exports the script uses.
    pub(crate) fn exports(&self) -> Exports {
        let mut u = Unifier::new();
        let expected = self.instantiate(&mut u);
        let row = match u.resolve_ty(&expected) {
            Ty::Record(row) => Some(row),
            _ => None,
        };
        Exports { u, row }
    }
}

/// A module's exports held to the type the script uses them at.  One scratch
/// unifier spans the whole record, because export types share variables.
pub(crate) struct Exports {
    u: Unifier,
    row: Option<Row>,
}

impl Exports {
    /// Whether `scheme` can be the type the script uses export `name` at:
    /// `None` when the script uses no such export, else the printed types
    /// `(found, expected)` of a failure.
    pub(crate) fn fits(
        &mut self,
        name: &str,
        scheme: &Arc<Scheme>,
    ) -> Option<Result<(), (String, String)>> {
        let u = &mut self.u;
        let want = field_ty(u, self.row.as_ref()?, name)?;
        let scheme = super::reseed_weak(u, Arc::clone(scheme));
        let have = super::instantiate(u, &scheme);
        let expected = u.apply_ty(&want);
        Some(
            u.unify_ty(&have, &want)
                .map_err(|_| (scheme.to_string(), expected.to_string())),
        )
    }
}

/// `row`'s field `name`, if the row names it.
fn field_ty(u: &Unifier, row: &Row, name: &str) -> Option<Ty> {
    let mut cur = u.resolve_row(row);
    loop {
        match cur {
            Row::Extend(label, ty, rest) => {
                if label == Label::Field(name.to_owned()) {
                    return Some(*ty);
                }
                cur = u.resolve_row(&rest);
            }
            Row::Var(_) | Row::Empty => return None,
        }
    }
}

fn row_of(
    u: &mut Unifier,
    fields: &Fields,
    ty: &impl Fn(Id) -> Ty,
    label: fn(String) -> Label,
    rows: &mut HashMap<u32, RowVar>,
) -> Row {
    let tail = match fields.tail {
        Tail::Closed => Row::Empty,
        Tail::Open { var, deep, weak } => {
            let row = Row::Var(
                *rows
                    .entry(var)
                    .or_insert_with(|| u.fresh_deep_row_var(deep)),
            );
            if weak {
                u.mark_weak(&Ty::Record(row.clone()), &WeakSource::Residual);
            }
            row
        }
    };
    fields.labels.iter().rev().fold(tail, |rest, (name, id)| {
        Row::Extend(label(name.clone()), Box::new(ty(*id)), Box::new(rest))
    })
}

struct Build<'a> {
    u: &'a Unifier,
    nodes: Vec<Node>,
    tys: HashMap<TyVar, Id>,
    comps: HashMap<CompTyVar, Id>,
}

impl Build<'_> {
    fn alloc(&mut self) -> Id {
        self.nodes.push(Node {
            shape: Shape::Unit,
            witness: None,
        });
        self.nodes.len() - 1
    }

    /// One node per variable root, allocated before its structure is walked,
    /// so a cycle closes on it.
    fn ty(&mut self, ty: &Ty) -> Id {
        let u = self.u;
        let root = match ty {
            Ty::Var(v) => Some(u.ty_root(*v)),
            _ => None,
        };
        if let Some(&id) = root.and_then(|r| self.tys.get(&r)) {
            return id;
        }
        let id = self.alloc();
        if let Some(r) = root {
            self.tys.insert(r, id);
        }
        let (shape, witness) = match &*u.head_ty(ty) {
            Ty::Var(v) => {
                let kinded = u.kinded(*v);
                (
                    Shape::Free {
                        var: v.0,
                        kind: kinded.kind,
                        weak: u.is_weak_ty(*v),
                    },
                    kinded.witness,
                )
            }
            Ty::Unit => (Shape::Unit, None),
            Ty::Bytes => (Shape::Bytes, None),
            Ty::Bool => (Shape::Bool, None),
            Ty::Int => (Shape::Int, None),
            Ty::Float => (Shape::Float, None),
            Ty::String => (Shape::String, None),
            Ty::List(a) => (Shape::List(self.ty(a)), None),
            Ty::Map(a) => (Shape::Map(self.ty(a)), None),
            Ty::Handle(grade, a) => (
                Shape::Handle(GradeShape::of(u.resolve_grade(*grade)), self.ty(a)),
                None,
            ),
            Ty::Record(r) => (Shape::Record(self.fields(r)), None),
            Ty::Variant(r) => (Shape::Variant(self.fields(r)), None),
            Ty::Thunk(c) => (Shape::Thunk(self.comp(c)), None),
        };
        let witness = witness.or_else(|| root.and_then(|r| u.bound_witness(r)));
        self.nodes[id] = Node { shape, witness };
        id
    }

    fn comp(&mut self, cty: &CompTy) -> Id {
        let u = self.u;
        let root = match cty {
            CompTy::Var(v) => Some(u.comp_root(*v)),
            _ => None,
        };
        if let Some(&id) = root.and_then(|r| self.comps.get(&r)) {
            return id;
        }
        let id = self.alloc();
        if let Some(r) = root {
            self.comps.insert(r, id);
        }
        let shape = match &*u.head_comp_ty(cty) {
            CompTy::Var(v) => Shape::FreeComp {
                weak: u.is_weak_comp(*v),
            },
            CompTy::Return(grade, a) => {
                Shape::Return(GradeShape::of(u.resolve_grade(*grade)), self.ty(a))
            }
            CompTy::Fun(a, b) => Shape::Fun(self.ty(a), self.comp(b)),
        };
        self.nodes[id].shape = shape;
        id
    }

    fn fields(&mut self, row: &Row) -> Fields {
        let u = self.u;
        let mut labels: Vec<(String, Id)> = Vec::new();
        let mut cur = u.resolve_row(row);
        let tail = loop {
            match cur {
                Row::Extend(label, ty, rest) => {
                    if !labels.iter().any(|(name, _)| name == label.name()) {
                        let id = self.ty(&ty);
                        labels.push((label.name().to_owned(), id));
                    }
                    cur = u.resolve_row(&rest);
                }
                Row::Var(v) => {
                    break Tail::Open {
                        var: v.0,
                        deep: u.is_deep_row(v),
                        weak: u.is_weak_row(v),
                    };
                }
                Row::Empty => break Tail::Closed,
            }
        };
        Fields { labels, tail }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ty::{closed_record, closed_variant, open_record, open_variant};

    fn agrees_with_unification(mut u: Unifier, want: &[Ty; 2], have: &[Ty; 2], fits: bool) {
        let snapshot = Site::snapshot(
            &u,
            &closed_record(&[("a", want[0].clone()), ("b", want[1].clone())]),
            UnitId::fresh(),
        );
        let mut exports = snapshot.exports();
        for (i, name) in ["a", "b"].into_iter().enumerate() {
            let direct = u.unify_ty(&have[i], &want[i]).is_ok();
            let exported = exports
                .fits(name, &Arc::new(Scheme::mono(have[i].clone())))
                .expect("named export")
                .is_ok();
            // The first pair always unifies (its open row absorbs the extra label); the
            // second only when the tails are unshared or agree on `extra`.
            assert_eq!(direct, i == 0 || fits, "direct: {name}");
            assert_eq!(exported, direct, "snapshot: {name}");
        }
    }

    #[test]
    fn contract_schemes_preserve_weakness_in_every_sort() {
        for weak in [false, true] {
            let mut u = Unifier::new();
            let item = Ty::Var(u.fresh_kinded_var(crate::ty::Kind::COMPARABLE));
            let body = u.fresh_comp_ty();
            let row = u.fresh_row_var();
            let ty = closed_record(&[
                ("item", item),
                ("body", Ty::Thunk(Box::new(body))),
                ("row", open_record(&[], row)),
            ]);
            if weak {
                u.mark_weak(&ty, &WeakSource::Residual);
            }
            let scheme = Site::snapshot(&u, &ty, UnitId::fresh()).scheme();
            assert_eq!(scheme.ty_vars.len(), usize::from(!weak));
            assert_eq!(scheme.comp_ty_vars.len(), usize::from(!weak));
            assert_eq!(scheme.row_vars.len(), usize::from(!weak));
            assert_eq!(scheme.weak.tys.len(), usize::from(weak));
            assert_eq!(scheme.weak.comps.len(), usize::from(weak));
            assert_eq!(scheme.weak.rows.len(), usize::from(weak));
        }
    }

    #[test]
    fn exports_preserve_shared_row_tails() {
        for variant in [false, true] {
            for shared in [false, true] {
                for same in [false, true] {
                    let mut u = Unifier::new();
                    let first = u.fresh_row_var();
                    let second = u.fresh_row_var();
                    let row = if variant { open_variant } else { open_record };
                    let want = [row(&[("known", Ty::Unit)], first), row(&[], second)];
                    if shared {
                        // Distinct ids united before snapshotting must share too.
                        u.unify_row(&Row::Var(first), &Row::Var(second)).unwrap();
                    }
                    let closed = if variant {
                        closed_variant
                    } else {
                        closed_record
                    };
                    let have = [
                        closed(&[("known", Ty::Unit), ("extra", Ty::Int)]),
                        closed(&[("extra", if same { Ty::Int } else { Ty::String })]),
                    ];
                    agrees_with_unification(u, &want, &have, !shared || same);
                }
            }
        }
    }

    #[test]
    fn exports_preserve_grades_shared_between_handles_and_returns() {
        for shared in [false, true] {
            for same in [false, true] {
                let mut u = Unifier::new();
                let first = u.fresh_grade_var();
                let second = u.fresh_grade_var();
                if shared {
                    u.bind_grade(first, Grade::Var(second));
                }
                let thunk = |grade| Ty::Thunk(Box::new(CompTy::Return(grade, Box::new(Ty::Unit))));
                let want = [
                    Ty::Handle(Grade::Var(first), Box::new(Ty::Unit)),
                    thunk(Grade::Var(second)),
                ];
                let have = [
                    Ty::Handle(Grade::Output, Box::new(Ty::Unit)),
                    thunk(if same { Grade::Output } else { Grade::Value }),
                ];
                agrees_with_unification(u, &want, &have, !shared || same);
            }
        }
    }
}
