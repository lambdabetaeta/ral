//! Building a [`Site`]: the checker freezes the type it solved at a boundary,
//! and re-instantiates it for the module door's scheme check.
//!
//! The graph keeps two things a `Ty` loses once the unifier has resolved it:
//! the span that imposed each structure, and the identity of each free
//! variable, which is what lets two decodes the program treats as one
//! `comparable` type agree ([`Fixings`]).  The runtime walks it in
//! `types::admit`.

use super::unify::Unifier;
use crate::ty::{
    CompTy, CompTyVar, Fields, Fixings, Grade, GradeShape, Id, Label, Node, Row, Scheme, Shape,
    Site, Tail, Ty, TyVar,
};
use std::collections::HashMap;
use std::sync::Arc;

impl GradeShape {
    fn of(grade: Grade) -> Self {
        match grade {
            Grade::Value => Self::Value,
            Grade::Output => Self::Output,
            Grade::Var(_) => Self::Free,
        }
    }

    fn instantiate(self, u: &mut Unifier) -> Grade {
        match self {
            Self::Value => Grade::Value,
            Self::Output => Grade::Output,
            Self::Free => u.fresh_grade(),
        }
    }
}

impl Site {
    /// Freeze `ty` as `u` has solved it.
    pub(crate) fn snapshot(u: &Unifier, ty: &Ty, fixings: Arc<Fixings>) -> Self {
        let mut build = Build {
            u,
            nodes: Vec::new(),
            tys: HashMap::new(),
            comps: HashMap::new(),
        };
        let root = build.ty(ty);
        Self {
            nodes: build.nodes,
            root,
            fixings,
        }
    }

    /// Whether both sites answer to one unit's [`Fixings`].
    #[cfg(test)]
    pub(crate) fn shares_fixings_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.fixings, &other.fixings)
    }

    /// The root as a type over `u`'s fresh variables: one variable per node,
    /// so sharing and cycles survive.
    fn instantiate(&self, u: &mut Unifier) -> Ty {
        let mut tys: HashMap<Id, TyVar> = HashMap::new();
        let mut comps: HashMap<Id, CompTyVar> = HashMap::new();
        for (id, node) in self.nodes.iter().enumerate() {
            match node.shape {
                Shape::Return(..) | Shape::Fun(..) | Shape::FreeComp => {
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
                Shape::Free { kind, .. } => {
                    let free = u.fresh_kinded_var(*kind);
                    u.bind_ty_root(tys[&id], Ty::Var(free));
                }
                Shape::FreeComp => {
                    let free = u.fresh_comp_ty();
                    u.bind_comp_root(comps[&id], free);
                }
                Shape::Return(grade, inner) => {
                    let grade = grade.instantiate(u);
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
                        Shape::Handle(e) => Ty::Handle(Box::new(ty(*e))),
                        Shape::Thunk(c) => Ty::Thunk(Box::new(comp(*c))),
                        Shape::Record(fields) => Ty::Record(row_of(u, fields, &ty, Label::Field)),
                        Shape::Variant(fields) => Ty::Variant(row_of(u, fields, &ty, Label::Case)),
                        Shape::Free { .. }
                        | Shape::Return(..)
                        | Shape::Fun(..)
                        | Shape::FreeComp => unreachable!("bound above"),
                    };
                    u.bind_ty_root(tys[&id], built);
                }
            }
        }
        ty(self.root)
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
) -> Row {
    let tail = match fields.tail {
        Tail::Closed => Row::Empty,
        Tail::Open { deep } => Row::Var(u.fresh_deep_row_var(deep)),
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
            Ty::Handle(a) => (Shape::Handle(self.ty(a)), None),
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
            CompTy::Var(_) => Shape::FreeComp,
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
                        deep: u.is_deep_row(v),
                    };
                }
                Row::Empty => break Tail::Closed,
            }
        };
        Fields { labels, tail }
    }
}
