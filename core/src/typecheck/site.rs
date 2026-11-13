//! Building a [`Site`]: the checker freezes the type it solved at a boundary.
//!
//! The graph keeps two things a `Ty` loses once the unifier has resolved it:
//! the span that imposed each structure, and the identity of each free
//! variable, which is what lets two decodes the program treats as one
//! `comparable` type agree (a session's [`crate::ty::Fixings`], under the
//! site's unit).  The runtime walks it in `shell::runtime::admit`.

use super::unify::Unifier;
use crate::ty::{
    CompTy, CompTyNode, CompTyVar, Fields, Grade, GradeShape, Id, Node, Row, RowNode, Shape, Site,
    Tail, Ty, TyNode, TyVar, UnitId,
};
use std::collections::HashMap;

impl GradeShape {
    fn of(grade: Grade) -> Self {
        match grade {
            Grade::Value => Self::Value,
            Grade::Output => Self::Output,
            Grade::Var(v) => Self::Free(v.0),
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
        let root = match ty.node() {
            TyNode::Var(v) => Some(u.ty_root(*v)),
            _ => None,
        };
        if let Some(&id) = root.and_then(|r| self.tys.get(&r)) {
            return id;
        }
        let id = self.alloc();
        if let Some(r) = root {
            self.tys.insert(r, id);
        }
        let (shape, witness) = match u.resolve_ty(ty).node() {
            TyNode::Var(v) => {
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
            TyNode::Unit => (Shape::Unit, None),
            TyNode::Bytes => (Shape::Bytes, None),
            TyNode::Bool => (Shape::Bool, None),
            TyNode::Int => (Shape::Int, None),
            TyNode::Float => (Shape::Float, None),
            TyNode::String => (Shape::String, None),
            TyNode::List(a) => (Shape::List(self.ty(a)), None),
            TyNode::Map(a) => (Shape::Map(self.ty(a)), None),
            TyNode::Handle(grade, a) => (
                Shape::Handle(GradeShape::of(u.resolve_grade(*grade)), self.ty(a)),
                None,
            ),
            TyNode::Record(r) => (Shape::Record(self.fields(r)), None),
            TyNode::Variant(r) => (Shape::Variant(self.fields(r)), None),
            TyNode::Thunk(c) => (Shape::Thunk(self.comp(c)), None),
        };
        let witness = witness.or_else(|| root.and_then(|r| u.bound_witness(r)));
        self.nodes[id] = Node { shape, witness };
        id
    }

    fn comp(&mut self, cty: &CompTy) -> Id {
        let u = self.u;
        let root = match cty.node() {
            CompTyNode::Var(v) => Some(u.comp_root(*v)),
            _ => None,
        };
        if let Some(&id) = root.and_then(|r| self.comps.get(&r)) {
            return id;
        }
        let id = self.alloc();
        if let Some(r) = root {
            self.comps.insert(r, id);
        }
        let shape = match u.resolve_comp_ty(cty).node() {
            CompTyNode::Var(v) => Shape::FreeComp {
                var: u.comp_root(*v).0,
                weak: u.is_weak_comp(*v),
            },
            CompTyNode::Return(grade, a) => {
                Shape::Return(GradeShape::of(u.resolve_grade(*grade)), self.ty(a))
            }
            CompTyNode::Fun(a, b) => Shape::Fun(self.ty(a), self.comp(b)),
        };
        self.nodes[id].shape = shape;
        id
    }

    fn fields(&mut self, row: &Row) -> Fields {
        let u = self.u;
        let mut labels: Vec<(String, Id)> = Vec::new();
        let mut cur = u.resolve_row(row);
        let tail = loop {
            match cur.node() {
                RowNode::Extend(label, ty, rest) => {
                    if !labels.iter().any(|(name, _)| name == label.name()) {
                        let id = self.ty(ty);
                        labels.push((label.name().to_owned(), id));
                    }
                    cur = u.resolve_row(rest);
                }
                RowNode::Var(v) => {
                    break Tail::Open {
                        var: v.0,
                        deep: u.is_deep_row(*v),
                        weak: u.is_weak_row(*v),
                    };
                }
                RowNode::Empty => break Tail::Closed,
            }
        };
        Fields { labels, tail }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::runtime::ValClos;
    use crate::source::{FileId, Span};
    use crate::ty::{Fixings, Kind, Label, closed_record, open_record, open_variant};
    use crate::typecheck::unify::WeakSource;

    #[test]
    fn recursive_row_payload_sites_preserve_cycles_residuals_and_comparable_identity() {
        for weak in [false, true] {
            let mut u = Unifier::new();
            u.enter();
            let rho = u.fresh_row_var();
            let residual = u.fresh_row_var();
            let item = u.fresh_kinded(Kind::Comparable);
            let inner = open_record(&[("item", item.clone()), ("mirror", item.clone())], rho);
            let proposed = open_record(&[("next", inner.clone())], residual);
            let TyNode::Record(proposed) = proposed.node() else {
                unreachable!()
            };
            u.unify_row(&Row::var(rho), proposed).unwrap();
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
        }
    }

    #[test]
    fn recursive_variant_sites_admit_finite_values() {
        let mut u = Unifier::new();
        let rho = u.fresh_row_var();
        let row = Row::extend(
            Label::Case("stop".into()),
            Ty::unit(),
            Row::extend(
                Label::Case("more".into()),
                Ty::variant(Row::var(rho)),
                Row::empty(),
            ),
        );
        u.unify_row(&Row::var(rho), &row).unwrap();
        let ty = Ty::variant(Row::var(rho));
        let site = Site::snapshot(&u, &ty, UnitId::fresh());
        let stop = ValClos::variant("stop", None);
        let more = |value| ValClos::variant("more", Some(value));
        let fixings = Fixings::default();
        site.admit(&stop, &fixings).unwrap();
        site.admit(&more(more(stop)), &fixings).unwrap();
        assert!(site.admit(&more(ValClos::int(1)), &fixings).is_err());
    }

    #[test]
    fn sites_preserve_shared_row_tails() {
        for variant in [false, true] {
            for shared in [false, true] {
                let mut u = Unifier::new();
                let first = u.fresh_row_var();
                let second = u.fresh_row_var();
                if shared {
                    u.unify_row(&Row::var(first), &Row::var(second)).unwrap();
                }
                let row = if variant { open_variant } else { open_record };
                let unit = UnitId::fresh();
                let a = Site::snapshot(&u, &row(&[("known", Ty::unit())], first), unit);
                let b = Site::snapshot(&u, &row(&[], second), unit);
                let tail = |site: &Site| match &site.nodes[site.root].shape {
                    Shape::Record(fields) | Shape::Variant(fields) => fields.tail,
                    _ => panic!("row site"),
                };
                assert!(matches!(tail(&a), Tail::Open { .. }));
                assert!(matches!(tail(&b), Tail::Open { .. }));
                assert_eq!(tail(&a) == tail(&b), shared);
            }
        }
    }

    #[test]
    fn sites_preserve_grades_shared_between_handles_and_returns() {
        for shared in [false, true] {
            let mut u = Unifier::new();
            let first = u.fresh_grade_var();
            let second = u.fresh_grade_var();
            if shared {
                u.bind_grade(first, Grade::Var(second));
            }
            let unit = UnitId::fresh();
            let handle = Site::snapshot(&u, &Ty::handle(Grade::Var(first), Ty::unit()), unit);
            let thunk = Site::snapshot(
                &u,
                &Ty::thunk(CompTy::ret(Grade::Var(second), Ty::unit())),
                unit,
            );
            let Shape::Handle(handle_grade, _) = handle.nodes[handle.root].shape else {
                panic!("handle site")
            };
            let Shape::Thunk(body) = thunk.nodes[thunk.root].shape else {
                panic!("thunk site")
            };
            let Shape::Return(return_grade, _) = thunk.nodes[body].shape else {
                panic!("return site")
            };
            assert_eq!(handle_grade == return_grade, shared);
        }
    }

    #[test]
    fn a_cyclic_type_snapshots_to_one_self_referring_node() {
        let mut u = Unifier::new();
        let a = u.fresh_ty();
        u.unify_ty(&a, &Ty::list(a.clone())).expect("a data cycle");
        let site = Site::snapshot(&u, &a, UnitId::fresh());
        assert_eq!(site.nodes[site.root].shape, Shape::List(site.root));
        assert!(
            site.admit(&ValClos::list(vec![]), &Fixings::default())
                .is_ok()
        );
        assert!(
            site.admit(&ValClos::list(vec![ValClos::int(1)]), &Fixings::default())
                .is_err()
        );
    }

    #[test]
    fn shared_comparable_variables_require_the_same_side() {
        for shared in [false, true] {
            let mut u = Unifier::new();
            let x = u.fresh_kinded(Kind::Comparable);
            let y = if shared {
                x.clone()
            } else {
                u.fresh_kinded(Kind::Comparable)
            };
            let ty = closed_record(&[("a", x), ("b", y)]);
            u.mark_weak(&ty, &WeakSource::Residual);
            let site = Site::snapshot(&u, &ty, UnitId::fresh());
            let value = ValClos::map(vec![
                ("a".into(), ValClos::int(1)),
                ("b".into(), ValClos::string("text")),
            ]);
            assert_eq!(site.admit(&value, &Fixings::default()).is_ok(), !shared);
        }
    }

    #[test]
    fn a_number_site_admits_numbers_not_text() {
        let mut u = Unifier::new();
        let free = u.fresh_kinded(Kind::Number);
        let site = Site::snapshot(&u, &free, UnitId::fresh());
        let fixings = Fixings::default();
        assert!(site.admit(&ValClos::int(1), &fixings).is_ok());
        assert!(site.admit(&ValClos::string("text"), &fixings).is_err());
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
        let kinded = u.fresh_kinded(Kind::Number);
        u.at = Some(fixed);
        let bound = u.fresh_ty();
        u.unify_ty(&bound, &Ty::int()).expect("a free variable");
        let site = Site::snapshot(
            &u,
            &closed_record(&[("k", kinded), ("b", bound)]),
            UnitId::fresh(),
        );
        let witnesses: Vec<_> = site.nodes.iter().map(|n| n.witness).collect();
        assert!(witnesses.contains(&Some(narrowed)), "{witnesses:?}");
        assert!(witnesses.contains(&Some(fixed)), "{witnesses:?}");
    }
}
