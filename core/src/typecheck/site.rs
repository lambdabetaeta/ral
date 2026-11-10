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
    Closed, CompTy, CompTyVar, Fields, Grade, GradeShape, Id, Label, Node, Row, RowVar, Shape,
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
                Shape::FreeComp { weak, .. } => {
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
    pub(crate) fn scheme(&self) -> Closed {
        let mut u = Unifier::new();
        u.enter();
        let ty = self.instantiate(&mut u);
        u.exit();
        super::generalize::settle_weak(&u, Arc::new(super::generalize::generalize(&u, &ty)))
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
        scheme: &Closed,
    ) -> Option<Result<(), (String, String)>> {
        let u = &mut self.u;
        let want = field_ty(u, self.row.as_ref()?, name)?;
        let scheme = super::reseed_weak(u, scheme);
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
                var: u.comp_root(*v).0,
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
    use crate::source::{FileId, Span};
    use crate::ty::{
        Binders, Kind, Role, closed_record, closed_variant, open_record, open_variant,
    };

    /// `ty` as `u` solved it, frozen and instantiated into a fresh unifier.
    fn thaw(u: &Unifier, ty: &Ty) -> (Unifier, Ty) {
        let mut fresh = Unifier::new();
        let thawed = Site::snapshot(u, ty, UnitId::fresh()).instantiate(&mut fresh);
        (fresh, thawed)
    }

    /// A record's fields in written order.
    fn fields_of(u: &Unifier, record: &Ty) -> Vec<Ty> {
        let Ty::Record(row) = u.resolve_ty(record) else {
            panic!("not a record: {record:?}");
        };
        row.fields().map(|(_, ty)| ty.clone()).collect()
    }

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
                .fits(name, &Binders::close(|_| have[i].clone()))
                .expect("named export")
                .is_ok();
            // The first pair always unifies (its open row absorbs the extra label); the
            // second only when the tails are unshared or agree on `extra`.
            assert_eq!(direct, i == 0 || fits, "direct: {name}");
            assert_eq!(exported, direct, "snapshot: {name}");
        }
    }

    #[test]
    fn recursive_row_payload_sites_preserve_cycles_residuals_and_comparable_identity() {
        for weak in [false, true] {
            let mut u = Unifier::new();
            u.enter();
            let rho = u.fresh_row_var();
            let residual = u.fresh_row_var();
            let item = u.fresh_kinded(Kind::COMPARABLE);
            let inner = open_record(&[("item", item.clone()), ("mirror", item.clone())], rho);
            let Ty::Record(proposed) = open_record(&[("next", inner.clone())], residual) else {
                unreachable!()
            };
            u.unify_row(&Row::Var(rho), &proposed).unwrap();
            if weak {
                u.mark_weak(&inner, &WeakSource::Residual);
            }
            let unit = UnitId::fresh();
            let site = Site::snapshot(&u, &inner, unit);
            let sibling = Site::snapshot(&u, &item, unit);
            let bytes = postcard::to_allocvec(&site).unwrap();
            let stored: Site = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(site, stored);
            let mut node = site.root;
            for _ in 0..4 {
                let Shape::Record(fields) = &site.nodes[node].shape else {
                    panic!("record node")
                };
                let field = |name: &str| {
                    fields
                        .labels
                        .iter()
                        .find(|(label, _)| label == name)
                        .unwrap()
                        .1
                };
                assert_eq!(fields.labels.len(), 3);
                assert_eq!(field("item"), field("mirror"));
                assert_eq!(
                    site.nodes[field("item")].shape,
                    sibling.nodes[sibling.root].shape
                );
                assert!(
                    matches!(fields.tail, Tail::Open { var, deep: false, weak: w } if var == residual.0 && w == weak)
                );
                node = field("next");
                let Shape::Record(next_fields) = &site.nodes[node].shape else {
                    panic!("recursive record node")
                };
                assert_eq!(
                    next_fields
                        .labels
                        .iter()
                        .find(|(label, _)| label == "next")
                        .unwrap()
                        .1,
                    node
                );
            }
            assert!(site.same_unit(&sibling));
            let mut next = Unifier::new();
            next.enter();
            let first = stored.instantiate(&mut next);
            let second = stored.instantiate(&mut next);
            for (instance, rigid) in [(&first, Ty::Int), (&second, Ty::String)] {
                let Ty::Record(row) = next.resolve_ty(instance) else {
                    panic!("record")
                };
                let fields = row
                    .fields()
                    .map(|(l, ty)| (l.name().to_owned(), ty.clone()))
                    .collect::<HashMap<_, _>>();
                next.unify_ty(&fields["item"], &rigid).unwrap();
                assert_eq!(next.apply_ty(&fields["mirror"]), rigid);
                let Ty::Record(recursive) = next.resolve_ty(&fields["next"]) else {
                    panic!("recursive record")
                };
                let nested = recursive
                    .fields()
                    .map(|(l, ty)| (l.name().to_owned(), ty.clone()))
                    .collect::<HashMap<_, _>>();
                assert_eq!(next.apply_ty(&nested["item"]), rigid);
                let round_trip =
                    Site::snapshot(&next, instance, UnitId::fresh()).instantiate(&mut next);
                next.unify_ty(instance, &round_trip).unwrap();
            }
            assert!(next.unify_ty(&first, &second).is_err());
        }
    }

    #[test]
    fn inhabited_recursive_variant_row_payloads_round_trip() {
        let mut u = Unifier::new();
        let rho = u.fresh_row_var();
        let row = Row::Extend(
            Label::Case("stop".into()),
            Box::new(Ty::Unit),
            Box::new(Row::Extend(
                Label::Case("more".into()),
                Box::new(Ty::Variant(Row::Var(rho))),
                Box::new(Row::Empty),
            )),
        );
        u.unify_row(&Row::Var(rho), &row).unwrap();
        let ty = Ty::Variant(Row::Var(rho));
        let site = Site::snapshot(&u, &ty, UnitId::fresh());
        let stop = crate::shell::runtime::Value::Variant {
            label: "stop".into(),
            payload: None,
        };
        let more = |value| crate::shell::runtime::Value::Variant {
            label: "more".into(),
            payload: Some(Box::new(value)),
        };
        let fixings = crate::ty::Fixings::default();
        site.admit(&stop, &fixings).unwrap();
        site.admit(&more(more(stop.clone())), &fixings).unwrap();
        assert!(
            site.admit(&more(crate::shell::runtime::Value::Int(1)), &fixings)
                .is_err()
        );
        let (mut next, imported) = thaw(&u, &ty);
        next.unify_ty(
            &imported,
            &closed_variant(&[("stop", Ty::Unit), ("more", imported.clone())]),
        )
        .unwrap();
        assert_ne!(next.cyclic_roots_in_ty(&imported).1, []);
        let (mut last, round_trip) = thaw(&next, &imported);
        Site::snapshot(&last, &round_trip, UnitId::fresh())
            .admit(&more(stop), &fixings)
            .unwrap();
        assert!(
            last.unify_ty(
                &round_trip,
                &closed_variant(&[("stop", Ty::Int), ("more", round_trip.clone())])
            )
            .is_err()
        );
    }

    #[test]
    fn contract_schemes_preserve_weakness_in_every_sort() {
        for weak in [false, true] {
            let mut u = Unifier::new();
            u.enter();
            let item = Ty::Var(u.fresh_kinded_var(crate::ty::Kind::COMPARABLE));
            let body = u.fresh_comp_ty();
            let row = u.fresh_row_var();
            let ty = closed_record(&[
                ("item", item),
                ("body", Ty::Thunk(Box::new(body))),
                ("row", open_record(&[], row)),
                ("grade", Ty::Handle(u.fresh_grade(), Box::new(Ty::Unit))),
            ]);
            if weak {
                u.mark_weak(&ty, &WeakSource::Residual);
            }
            let scheme = Site::snapshot(&u, &ty, UnitId::fresh()).scheme();
            assert_eq!(
                (
                    scheme.tys.values().map(Role::is_weak).collect::<Vec<_>>(),
                    scheme.comps.values().map(Role::is_weak).collect::<Vec<_>>(),
                    scheme.rows.values().map(Role::is_weak).collect::<Vec<_>>(),
                    scheme.grades.len(),
                ),
                (vec![weak], vec![weak], vec![weak], 1)
            );
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

    #[test]
    fn a_cyclic_type_snapshots_to_one_self_referring_node() {
        let mut u = Unifier::new();
        let a = u.fresh_ty();
        u.unify_ty(&a, &Ty::list(a.clone())).expect("a data cycle");
        let site = Site::snapshot(&u, &a, UnitId::fresh());
        assert_eq!(site.nodes.len(), 1);
        assert_eq!(site.nodes[0].shape, Shape::List(0));

        let mut v = Unifier::new();
        let Ty::Var(root) = site.instantiate(&mut v) else {
            panic!("the root of a cycle is a variable");
        };
        let Ty::List(elem) = v.resolve_ty(&Ty::Var(root)) else {
            panic!("the root stands for a list");
        };
        let Ty::Var(inner) = *elem else {
            panic!("the element closes the cycle on a variable");
        };
        assert_eq!(v.ty_root(inner), v.ty_root(root));
    }

    /// A variable used twice is one variable after the round trip.
    #[test]
    fn a_shared_variable_stays_shared_and_distinct_ones_stay_apart() {
        for shared in [false, true] {
            let mut u = Unifier::new();
            let x = u.fresh_ty();
            let y = if shared { x.clone() } else { u.fresh_ty() };
            let (mut v, ty) = thaw(&u, &closed_record(&[("a", x), ("b", y)]));
            let [a, b] = &fields_of(&v, &ty)[..] else {
                panic!("two fields");
            };
            v.unify_ty(a, &Ty::Int).expect("a free variable");
            assert_eq!(matches!(v.apply_ty(b), Ty::Int), shared);
        }
    }

    #[test]
    fn functions_returns_and_handles_round_trip() {
        let block = |cty| Ty::Thunk(Box::new(cty));
        let tys = [
            block(CompTy::pure(Ty::Int)),
            block(CompTy::arrows(
                [Ty::Int, Ty::String],
                CompTy::Return(Grade::Output, Box::new(Ty::Unit)),
            )),
            block(CompTy::arrows(
                [block(CompTy::pure(Ty::Bool))],
                CompTy::pure(Ty::list(Ty::Float)),
            )),
            Ty::Handle(Grade::Output, Box::new(Ty::map(Ty::Bytes))),
        ];
        for ty in tys {
            let (v, thawed) = thaw(&Unifier::new(), &ty);
            assert_eq!(v.apply_ty(&thawed), ty);
        }
    }

    #[test]
    fn a_kinded_variable_keeps_its_kind() {
        let block = Ty::Thunk(Box::new(CompTy::pure(Ty::Unit)));
        for (kind, admitted, refused) in [
            (Kind::NUMBER, Ty::Int, Ty::String),
            (Kind::DATA, Ty::Int, block),
        ] {
            let mut u = Unifier::new();
            let free = u.fresh_kinded(kind);
            let (mut v, thawed) = thaw(&u, &free);
            assert!(v.unify_ty(&thawed, &refused).is_err(), "{kind:?}");
            let (mut v, thawed) = thaw(&u, &free);
            assert!(v.unify_ty(&thawed, &admitted).is_ok(), "{kind:?}");
        }
    }

    /// A node remembers the use that narrowed its kind or fixed its structure.
    #[test]
    fn witnesses_are_the_spans_that_imposed_the_node() {
        let (narrowed, fixed) = (
            Span::new(FileId::DUMMY, 1, 2),
            Span::new(FileId::DUMMY, 5, 6),
        );
        let mut u = Unifier::new();
        u.at = Some(narrowed);
        let kinded = u.fresh_kinded(Kind::NUMBER);
        u.at = Some(fixed);
        let bound = u.fresh_ty();
        u.unify_ty(&bound, &Ty::Int).expect("a free variable");
        let site = Site::snapshot(
            &u,
            &closed_record(&[("k", kinded), ("b", bound)]),
            UnitId::fresh(),
        );
        let witnesses: Vec<_> = site.nodes.iter().map(|n| n.witness).collect();
        assert!(witnesses.contains(&Some(narrowed)), "{witnesses:?}");
        assert!(witnesses.contains(&Some(fixed)), "{witnesses:?}");
    }

    /// A script that uses no such export, or no record at all, has nothing to
    /// check an export against.
    #[test]
    fn exports_without_a_named_field_have_nothing_to_fit() {
        let mut u = Unifier::new();
        let tail = u.fresh_row_var();
        let int = Binders::close(|_| Ty::Int);
        for (ty, name) in [
            (Ty::Int, "a"),
            (closed_record(&[("a", Ty::Int)]), "other"),
            (open_record(&[("a", Ty::Int)], tail), "other"),
        ] {
            let mut exports = Site::snapshot(&u, &ty, UnitId::fresh()).exports();
            assert!(exports.fits(name, &int).is_none(), "{ty:?} / {name}");
        }
        let mut exports =
            Site::snapshot(&u, &closed_record(&[("a", Ty::Int)]), UnitId::fresh()).exports();
        assert!(matches!(exports.fits("a", &int), Some(Ok(()))));
        let string = Binders::close(|_| Ty::String);
        assert!(matches!(exports.fits("a", &string), Some(Err(_))));
    }
}
