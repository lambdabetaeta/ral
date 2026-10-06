//! Union-find unifier over four variable kinds: type, computation type, row,
//! grade.
//!
//! Value and computation types are both *equi-recursive* — a slot may be bound
//! to a structure containing its own variable — provided every cycle crosses
//! data (a list, map, record or variant): binding refuses a structure that
//! reaches its own variable along `U`, `→`, `F` and `Handle` alone.  Every
//! traversal carries a [`Visited`] of the roots it is
//! expanding, turning a cycle into a back-edge, and unification carries a
//! co-inductive [`Pairs`], so two cyclic types reach a fixed point.

use super::error::{CycleVia, KindFound, TypeErrorKind};
use super::generalize::{FreeVars, free_ty};
use crate::source::Span;
use crate::ty::{
    CompTy, CompTyVar, Grade, GradeVar, Head, Kind, Label, Row, RowVar, Term, Ty, TyVar, WeakVars,
};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::sync::Arc;

/// What made a variable weak, so a mismatch on it can say which use shares it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WeakSource {
    /// A computed index `$c[$k]`.
    Index,
    /// The result of a boundary, named as the program spells it.
    Boundary(Arc<str>),
    /// A residual an earlier unit stored, whose own source is gone.
    Residual,
}

/// Cycle-tracking state, threaded through `apply_*` and `reach`.  `tys`/`comps`
/// are a stack for `apply_*` and a set for `reach`, whose visits are
/// idempotent.  A root that proves cyclic goes
/// into `cyclic_*` and stays visited for the rest of the call, so siblings
/// sharing that subtree keep getting back-edges — while a non-cyclic root stays
/// out, letting a sibling `τ → Int` root re-resolve to `Int`.
#[derive(Default)]
pub(super) struct Visited {
    pub(crate) tys: HashSet<TyVar>,
    pub(crate) comps: HashSet<CompTyVar>,
    cyclic_tys: HashSet<TyVar>,
    cyclic_comps: HashSet<CompTyVar>,
    /// Leave a weak variable as itself instead of what it was fixed to.
    keep_weak: bool,
}

/// A variable met on a walk, at its root.
#[derive(Clone, Copy)]
pub(super) enum Var {
    Ty(TyVar),
    Comp(CompTyVar),
    Row(RowVar),
    Grade(GradeVar),
}

/// The free variables a `data` obligation lands on.
#[derive(Default)]
struct Owed {
    tys: Vec<TyVar>,
    rows: Vec<RowVar>,
}

/// The variable a binding must not reach again without crossing data.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Anchor {
    Ty(TyVar),
    Comp(CompTyVar),
}

/// Where the data-free search starts: the structure about to be bound.
#[derive(Clone, Copy)]
enum Reach<'a> {
    Ty(&'a Ty),
    Comp(&'a CompTy),
}

/// Equality obligations already in progress; re-entering one is an immediate
/// success, which is what makes two cyclic types terminate.  Alongside the
/// symmetric root pairs are *one-sided* obligations — a root against a
/// [`TyKey`] / [`CompTyKey`] of the other side — because a value-anchored
/// stream (`T = Step(F T)`) meets its comp-anchored twin (`C = F Step(C)`) as
/// `Var`-vs-structure, never as a `Var`/`Var` pair.
#[derive(Default)]
struct Pairs {
    tys: HashSet<(TyVar, TyVar)>,
    comps: HashSet<(CompTyVar, CompTyVar)>,
    ty_expansions: HashSet<(TyVar, TyKey)>,
    comp_expansions: HashSet<(CompTyVar, CompTyKey)>,
}

/// Ceiling on *true nesting depth*: descents into a strictly deeper subterm.
/// Walking a row spine sideways is width and costs nothing, so a wide-but-
/// shallow record unifies freely.  [`Pairs`] terminates every *cyclic*
/// obligation; this is the structural stop for a variable-free type, turning a
/// stack overflow into a graceful [`TypeErrorKind::TypeTooDeep`].  The unify
/// arms, the key fingerprints and the row occurs check spend the one budget, so
/// no descent escapes by crossing between them.  The value sits far above real
/// nesting yet under the ~900 frames that exhaust a 2 MiB stack.
const MAX_UNIFY_DEPTH: u32 = 512;

/// Charge one level against [`MAX_UNIFY_DEPTH`].  Called only on a step into a
/// strictly deeper subterm, which is why `depth` is threaded by hand below.
fn deeper(depth: u32) -> Result<u32, TypeErrorKind> {
    if depth >= MAX_UNIFY_DEPTH {
        return Err(TypeErrorKind::TypeTooDeep);
    }
    Ok(depth + 1)
}

/// Fingerprint of a value type: the non-variable half of a one-sided obligation.
/// A variable collapses to its root and the walk stops, so the key stays finite
/// even over a root bound to a cyclic structure.
#[derive(Clone, PartialEq, Eq, Hash)]
enum TyKey {
    Unit,
    Bytes,
    Bool,
    Int,
    Float,
    String,
    List(Box<Self>),
    Map(Box<Self>),
    Record(RowKey),
    Variant(RowKey),
    Thunk(Box<CompTyKey>),
    Handle(Box<Self>),
    Var(TyVar),
}

/// Fingerprint of a computation type.  See [`TyKey`].  The grade is stored
/// resolved, a variable at its root.
#[derive(Clone, PartialEq, Eq, Hash)]
enum CompTyKey {
    Return(Grade, Box<TyKey>),
    Fun(Box<TyKey>, Box<Self>),
    Var(CompTyVar),
}

/// Fingerprint of a row spine.  See [`TyKey`].
#[derive(Clone, PartialEq, Eq, Hash)]
enum RowKey {
    Empty,
    Var(RowVar),
    Extend(Label, Box<TyKey>, Box<Self>),
}

/// A union-find variable of one sort: the id and nothing else, so a root of
/// one store cannot index another's.
trait VarId: Copy + Eq + std::hash::Hash {
    fn raw(self) -> u32;
    fn new(raw: u32) -> Self;
}

macro_rules! var_id {
    ($($var:ident),*) => {$(
        impl VarId for $var {
            fn raw(self) -> u32 {
                self.0
            }
            fn new(raw: u32) -> Self {
                Self(raw)
            }
        }
    )*};
}
var_id!(TyVar, CompTyVar, RowVar, GradeVar);

/// A sort with variables in a union-find store: `as_var` projects the variable
/// out of a bare one, `from_root` rebuilds one at a canonical root.
trait Unifiable: Clone {
    type Var: VarId;
    fn as_var(&self) -> Option<Self::Var>;
    fn from_root(root: Self::Var) -> Self;
}

macro_rules! unifiable {
    ($($ty:ident => $var:ident),*) => {$(
        impl Unifiable for $ty {
            type Var = $var;
            fn as_var(&self) -> Option<$var> {
                match self {
                    Self::Var(v) => Some(*v),
                    _ => None,
                }
            }
            fn from_root(root: $var) -> Self {
                Self::Var(root)
            }
        }
    )*};
}
unifiable!(Ty => TyVar, CompTy => CompTyVar, Row => RowVar, Grade => GradeVar);

#[derive(Clone)]
enum Slot<T: Unifiable> {
    Free,
    Bound(T),
    Parent(T::Var),
}

#[derive(Clone)]
struct Store<T: Unifiable> {
    slots: Vec<Slot<T>>,
    next: u32,
    /// Roots that are weak: permanent for the unit, and inherited by whatever
    /// a weak variable is united with.
    weak: HashMap<T::Var, WeakSource>,
}

impl<T: Unifiable> Store<T> {
    fn new() -> Self {
        Self {
            slots: Vec::new(),
            next: 0,
            weak: HashMap::new(),
        }
    }

    fn fresh(&mut self) -> T::Var {
        let id = self.next;
        self.next += 1;
        self.slots.push(Slot::Free);
        T::Var::new(id)
    }

    fn find(&mut self, v: T::Var) -> T::Var {
        // Out-of-range ids belong to a foreign unifier — cached prelude schemes
        // loaded into a fresh `InferCtx`.  Treat them as free.
        if v.raw() as usize >= self.slots.len() {
            return v;
        }
        match self.slots[v.raw() as usize] {
            Slot::Parent(p) => {
                let r = self.find(p);
                self.slots[v.raw() as usize] = Slot::Parent(r);
                r
            }
            _ => v,
        }
    }

    /// [`Self::find`] without path compression, for the read-only traversals.
    fn root(&self, mut v: T::Var) -> T::Var {
        while let Some(&Slot::Parent(p)) = self.slots.get(v.raw() as usize) {
            v = p;
        }
        v
    }

    fn get_ref(&self, v: T::Var) -> Option<&T> {
        match self.slots.get(self.root(v).raw() as usize)? {
            Slot::Bound(t) => Some(t),
            _ => None,
        }
    }

    /// Grow to cover `v`: cached prelude vars can arrive above a fresh `next`.
    fn ensure(&mut self, v: T::Var) {
        let needed = (v.raw() as usize) + 1;
        if needed > self.slots.len() {
            self.slots.resize_with(needed, || Slot::Free);
            #[allow(
                clippy::cast_possible_truncation,
                reason = "needed = i+1 for a u32 var-id i; var-ids never approach 2^32"
            )]
            if needed as u32 > self.next {
                self.next = needed as u32;
            }
        }
    }

    fn bind(&mut self, v: T::Var, val: T) {
        self.ensure(v);
        self.slots[v.raw() as usize] = Slot::Bound(val);
    }

    fn union(&mut self, a: T::Var, b: T::Var) {
        self.ensure(T::Var::new(a.raw().max(b.raw())));
        self.slots[a.raw() as usize] = Slot::Parent(b);
    }

    fn unite(&mut self, a: T::Var, b: T::Var) {
        if a == b {
            return;
        }
        let ar = self.find(a);
        let br = self.find(b);
        if ar != br {
            self.union(ar, br);
            if let Some(source) = self.weak.get(&ar).cloned() {
                self.weak.entry(br).or_insert(source);
            }
        }
    }

    fn mark_weak(&mut self, v: T::Var, source: WeakSource) {
        let root = self.find(v);
        self.weak.entry(root).or_insert(source);
    }

    fn weak_source(&self, v: T::Var) -> Option<&WeakSource> {
        self.weak.get(&self.root(v))
    }

    fn is_weak(&self, v: T::Var) -> bool {
        self.weak_source(v).is_some()
    }

    /// Follow a variable chain to canonical form.  The walk stops at the first
    /// non-variable head, so variables nested inside it — a cyclic binding's
    /// back-edges — survive untouched.  The head is borrowed; only a free
    /// variable is rebuilt, at its root.
    fn resolve<'a>(&'a self, x: &'a T) -> Cow<'a, T> {
        match x.as_var() {
            Some(v) => match self.get_ref(v) {
                Some(b) => self.resolve(b),
                None => Cow::Owned(T::from_root(self.root(v))),
            },
            None => Cow::Borrowed(x),
        }
    }
}

/// The kind a free type variable carries, and the span that narrowed it.
#[derive(Clone, Copy)]
pub(crate) struct Kinded {
    pub(crate) kind: Kind,
    pub(crate) witness: Option<Span>,
}

#[derive(Clone)]
pub struct Unifier {
    tys: Store<Ty>,
    ctys: Store<CompTy>,
    rows: Store<Row>,
    /// Grades are atomic and never weak: no kind, no cycle, no `weak` entry.
    grades: Store<Grade>,
    /// The kind of each free type-variable root, where not `any`.
    kinds: HashMap<TyVar, Kinded>,
    /// The free row-variable roots that are deep, with the span that made them so.
    deep_rows: HashMap<RowVar, Option<Span>>,
    /// The span in force when a type-variable root was first bound to a
    /// structure: the use that imposed it.
    bound_at: HashMap<TyVar, Span>,
    /// The span in force: the witness of any kind a unification narrows.
    pub(crate) at: Option<Span>,
}

impl Unifier {
    pub fn new() -> Self {
        Self {
            tys: Store::new(),
            ctys: Store::new(),
            rows: Store::new(),
            grades: Store::new(),
            kinds: HashMap::new(),
            deep_rows: HashMap::new(),
            bound_at: HashMap::new(),
            at: None,
        }
    }

    pub fn fresh_tyvar(&mut self) -> TyVar {
        self.tys.fresh()
    }
    pub(crate) fn fresh_ty(&mut self) -> Ty {
        Ty::Var(self.fresh_tyvar())
    }

    pub(crate) fn fresh_comp_ty(&mut self) -> CompTy {
        CompTy::Var(self.ctys.fresh())
    }
    pub fn fresh_row_var(&mut self) -> RowVar {
        self.rows.fresh()
    }
    pub(crate) fn fresh_row(&mut self) -> Row {
        Row::Var(self.fresh_row_var())
    }

    pub fn fresh_grade_var(&mut self) -> GradeVar {
        self.grades.fresh()
    }
    pub(crate) fn fresh_grade(&mut self) -> Grade {
        Grade::Var(self.fresh_grade_var())
    }

    pub(crate) fn resolve_grade(&self, grade: Grade) -> Grade {
        self.grades.resolve(&grade).into_owned()
    }

    /// Bind a free grade variable.  Its root is taken here, so the caller
    /// may hand in any variable of the class.
    pub(crate) fn bind_grade(&mut self, v: GradeVar, grade: Grade) {
        let root = self.grades.find(v);
        self.grades.bind(root, grade);
    }

    /// Unify two grades; `false` when they are distinct constants.  Atomic,
    /// so there is no structure to descend and nothing to report but the two.
    fn unify_grade(&mut self, a: Grade, b: Grade) -> bool {
        match (self.resolve_grade(a), self.resolve_grade(b)) {
            (Grade::Var(x), Grade::Var(y)) => {
                self.grades.unite(x, y);
                true
            }
            (Grade::Var(v), grade) | (grade, Grade::Var(v)) => {
                self.bind_grade(v, grade);
                true
            }
            (a, b) => a == b,
        }
    }

    /// A fresh variable of `kind`, narrowed at the span in force.
    pub(crate) fn fresh_kinded_var(&mut self, kind: Kind) -> TyVar {
        let v = self.fresh_tyvar();
        self.install_kind(
            v,
            Kinded {
                kind,
                witness: self.at,
            },
        );
        v
    }

    pub(crate) fn fresh_kinded(&mut self, kind: Kind) -> Ty {
        Ty::Var(self.fresh_kinded_var(kind))
    }

    pub(crate) fn fresh_deep_row_var(&mut self, deep: bool) -> RowVar {
        let v = self.fresh_row_var();
        if deep {
            self.deep_rows.insert(v, self.at);
        }
        v
    }

    /// The kind a free variable carries, and the span that narrowed it.
    pub(crate) fn kinded(&self, v: TyVar) -> Kinded {
        self.kind_at(self.tys.root(v))
    }

    fn kind_at(&self, root: TyVar) -> Kinded {
        self.kinds.get(&root).copied().unwrap_or(Kinded {
            kind: Kind::ANY,
            witness: None,
        })
    }

    pub(crate) fn is_deep_row(&self, v: RowVar) -> bool {
        self.deep_rows.contains_key(&self.rows.root(v))
    }

    /// The use that fixed the structure a type-variable root stands for.
    pub(crate) fn bound_witness(&self, root: TyVar) -> Option<Span> {
        self.bound_at.get(&root).copied()
    }

    /// Record `kinded` on a free root.  A meet that leaves one nullary head
    /// makes the variable that type.
    fn install_kind(&mut self, root: TyVar, kinded: Kinded) {
        match kinded.kind.pin() {
            Some(ty) => {
                self.kinds.remove(&root);
                self.tys.bind(root, ty);
            }
            None if kinded.kind.is_any() => {
                self.kinds.remove(&root);
            }
            None => {
                self.kinds.insert(root, kinded);
            }
        }
    }

    fn unite_tys(&mut self, a: TyVar, b: TyVar) -> Result<(), TypeErrorKind> {
        let (ar, br) = (self.tys.find(a), self.tys.find(b));
        if ar == br {
            return Ok(());
        }
        let (ka, kb) = (self.kind_at(ar), self.kind_at(br));
        let Some(kind) = ka.kind.meet(kb.kind) else {
            return Err(TypeErrorKind::KindMismatch {
                found: KindFound::Used {
                    kind: ka.kind,
                    witness: ka.witness,
                },
                kind: kb.kind,
                witness: kb.witness,
            });
        };
        let witness = if kind == ka.kind {
            ka.witness
        } else {
            kb.witness
        };
        self.tys.unite(ar, br);
        self.kinds.remove(&ar);
        self.install_kind(br, Kinded { kind, witness });
        Ok(())
    }

    fn unite_rows(&mut self, a: RowVar, b: RowVar) {
        let (ar, br) = (self.rows.find(a), self.rows.find(b));
        if ar == br {
            return;
        }
        let deep = self.deep_rows.remove(&ar);
        self.rows.unite(ar, br);
        if let Some(witness) = deep {
            self.deep_rows.entry(br).or_insert(witness);
        }
    }

    /// Narrow a free variable to the meet of its kind and `kind`.
    fn narrow(
        &mut self,
        root: TyVar,
        kind: Kind,
        witness: Option<Span>,
    ) -> Result<(), TypeErrorKind> {
        let root = self.tys.find(root);
        let current = self.kind_at(root);
        let Some(meet) = current.kind.meet(kind) else {
            return Err(TypeErrorKind::KindMismatch {
                found: KindFound::Used {
                    kind: current.kind,
                    witness: current.witness,
                },
                kind,
                witness,
            });
        };
        if meet != current.kind {
            self.install_kind(
                root,
                Kinded {
                    kind: meet,
                    witness,
                },
            );
        }
        Ok(())
    }

    fn kind_error(&self, found: &Ty, kind: Kind, witness: Option<Span>) -> TypeErrorKind {
        TypeErrorKind::KindMismatch {
            found: KindFound::Type(Box::new(self.apply_ty(found))),
            kind,
            witness,
        }
    }

    /// Whether `val` may become the structure `root` stands for: its head is
    /// one the kind admits, and a deep kind's components are data.
    fn admit(&mut self, root: TyVar, val: &Ty, depth: u32) -> Result<(), TypeErrorKind> {
        let Some(Kinded { kind, witness }) = self.kinds.get(&root).copied() else {
            return Ok(());
        };
        if Head::of(val).is_some_and(|head| !kind.admits(head)) {
            return Err(self.kind_error(val, kind, witness));
        }
        if kind.is_deep() {
            let mut owed = Owed::default();
            let mut seen = HashSet::from([root]);
            self.data_ty(val, witness, &mut seen, &mut owed, depth)?;
            self.impose_data(owed, witness)?;
        }
        self.kinds.remove(&root);
        Ok(())
    }

    /// The same, for a deep row variable about to become `val`.
    fn admit_row(&mut self, root: RowVar, val: &Row, depth: u32) -> Result<(), TypeErrorKind> {
        let Some(&witness) = self.deep_rows.get(&root) else {
            return Ok(());
        };
        let mut owed = Owed::default();
        self.data_row(val, witness, &mut HashSet::new(), &mut owed, depth)?;
        self.impose_data(owed, witness)?;
        self.deep_rows.remove(&root);
        Ok(())
    }

    /// Walk `ty` as data: refuse a block or handle, and collect the free
    /// variables the obligation lands on.  `seen` carries the roots expanded,
    /// so a cyclic type ends.
    fn data_ty(
        &self,
        ty: &Ty,
        witness: Option<Span>,
        seen: &mut HashSet<TyVar>,
        owed: &mut Owed,
        depth: u32,
    ) -> Result<(), TypeErrorKind> {
        if let Ty::Var(v) = ty
            && !seen.insert(self.tys.root(*v))
        {
            return Ok(());
        }
        let depth = deeper(depth)?;
        match &*self.head_ty(ty) {
            Ty::Var(v) => owed.tys.push(*v),
            Ty::Unit | Ty::Bool | Ty::Int | Ty::Float | Ty::String | Ty::Bytes => {}
            Ty::List(a) | Ty::Map(a) => self.data_ty(a, witness, seen, owed, depth)?,
            Ty::Record(r) | Ty::Variant(r) => self.data_row(r, witness, seen, owed, depth)?,
            found @ (Ty::Thunk(_) | Ty::Handle(_)) => {
                return Err(self.kind_error(found, Kind::DATA, witness));
            }
        }
        Ok(())
    }

    fn data_row(
        &self,
        row: &Row,
        witness: Option<Span>,
        seen: &mut HashSet<TyVar>,
        owed: &mut Owed,
        depth: u32,
    ) -> Result<(), TypeErrorKind> {
        match &*self.head_row(row) {
            Row::Empty => Ok(()),
            Row::Var(v) => {
                owed.rows.push(*v);
                Ok(())
            }
            Row::Extend(_, ty, rest) => {
                self.data_ty(ty, witness, seen, owed, depth)?;
                self.data_row(rest, witness, seen, owed, depth)
            }
        }
    }

    fn impose_data(&mut self, owed: Owed, witness: Option<Span>) -> Result<(), TypeErrorKind> {
        for root in owed.tys {
            self.narrow(root, Kind::DATA, witness)?;
        }
        for root in owed.rows {
            let root = self.rows.find(root);
            self.deep_rows.entry(root).or_insert(witness);
        }
        Ok(())
    }

    /// Make every variable free in `ty` weak: one type for the whole unit,
    /// never quantified, and inherited by whatever it is united with.
    pub(crate) fn mark_weak(&mut self, ty: &Ty, source: &WeakSource) {
        let mut fvs = FreeVars::new();
        free_ty(self, ty, &mut fvs);
        for v in &fvs.tys {
            self.tys.mark_weak(*v, source.clone());
        }
        for v in &fvs.comps {
            self.ctys.mark_weak(*v, source.clone());
        }
        for v in &fvs.rows {
            self.rows.mark_weak(*v, source.clone());
        }
    }

    pub(crate) fn mark_weak_vars(&mut self, weak: &WeakVars) {
        for v in weak.tys.keys() {
            self.tys.mark_weak(*v, WeakSource::Residual);
        }
        for v in &weak.comps {
            self.ctys.mark_weak(*v, WeakSource::Residual);
        }
        for v in weak.rows.keys() {
            self.rows.mark_weak(*v, WeakSource::Residual);
        }
    }

    fn weak_var(&self, ty: &Ty) -> Option<WeakSource> {
        match ty {
            Ty::Var(v) => self.tys.weak_source(*v).cloned(),
            _ => None,
        }
    }

    fn weak_row_var(&self, row: &Row) -> Option<WeakSource> {
        match row {
            Row::Var(v) => self.rows.weak_source(*v).cloned(),
            _ => None,
        }
    }

    fn weak_comp_var(&self, cty: &CompTy) -> Option<WeakSource> {
        match cty {
            CompTy::Var(v) => self.ctys.weak_source(*v).cloned(),
            _ => None,
        }
    }

    pub(crate) fn is_weak_ty(&self, v: TyVar) -> bool {
        self.tys.is_weak(v)
    }

    pub(crate) fn is_weak_comp(&self, v: CompTyVar) -> bool {
        self.ctys.is_weak(v)
    }

    pub(crate) fn is_weak_row(&self, v: RowVar) -> bool {
        self.rows.is_weak(v)
    }

    /// Whether a weak variable stands anywhere in `ty`, bound ones included:
    /// a mismatch with the type a weak variable was fixed to is a mismatch on it.
    pub(crate) fn reaches_weak(&self, ty: &Ty) -> bool {
        self.weak_source_in_ty(ty).is_some()
    }

    /// What made the first weak variable in `ty` weak, bound ones included.
    pub(crate) fn weak_source_in_ty(&self, ty: &Ty) -> Option<WeakSource> {
        self.weak_source_in(Term::Ty(ty))
    }

    pub(crate) fn weak_source_in_comp(&self, cty: &CompTy) -> Option<WeakSource> {
        self.weak_source_in(Term::Comp(cty))
    }

    fn weak_source_in(&self, term: Term<'_>) -> Option<WeakSource> {
        let mut weak = |var: Var| match self.weak_source_of(var) {
            Some(source) => ControlFlow::Break(source.clone()),
            None => ControlFlow::Continue(()),
        };
        self.reach(term, &mut Visited::default(), &mut weak)
            .break_value()
    }

    fn weak_source_of(&self, var: Var) -> Option<&WeakSource> {
        match var {
            Var::Ty(v) => self.tys.weak_source(v),
            Var::Comp(v) => self.ctys.weak_source(v),
            Var::Row(v) => self.rows.weak_source(v),
            Var::Grade(_) => None,
        }
    }

    /// The variable `term` is, at its root.
    fn var_of(&self, term: Term<'_>) -> Option<Var> {
        match term {
            Term::Ty(Ty::Var(v)) => Some(Var::Ty(self.tys.root(*v))),
            Term::Comp(CompTy::Var(v)) => Some(Var::Comp(self.ctys.root(*v))),
            Term::Row(Row::Var(v)) => Some(Var::Row(self.rows.root(*v))),
            _ => None,
        }
    }

    /// Whether `var`, a root, stands for nothing yet.
    pub(super) fn is_unbound(&self, var: Var) -> bool {
        match var {
            Var::Ty(v) => self.tys.get_ref(v).is_none(),
            Var::Comp(v) => self.ctys.get_ref(v).is_none(),
            Var::Row(v) => self.rows.get_ref(v).is_none(),
            Var::Grade(_) => true,
        }
    }

    /// Where a walk goes from `term`: through a bound variable to what it is
    /// bound to, otherwise into its sub-terms.
    fn successors<'a>(&'a self, term: Term<'a>) -> impl Iterator<Item = Term<'a>> {
        let binding = match self.var_of(term) {
            Some(Var::Ty(v)) => self.tys.get_ref(v).map(Term::Ty),
            Some(Var::Comp(v)) => self.ctys.get_ref(v).map(Term::Comp),
            Some(Var::Row(v)) => self.rows.get_ref(v).map(Term::Row),
            _ => None,
        };
        binding.into_iter().chain(term.children())
    }

    /// Every variable reachable from `term`, bound ones included, at each
    /// occurrence; `visit` may stop the walk.  Each type or computation root is
    /// followed to its binding once, so a cycle ends at its back-edge.
    pub(super) fn reach<'a, B>(
        &'a self,
        term: Term<'a>,
        seen: &mut Visited,
        visit: &mut impl FnMut(Var) -> ControlFlow<B>,
    ) -> ControlFlow<B> {
        if let Term::Comp(CompTy::Return(grade, _)) = term
            && let Grade::Var(v) = self.resolve_grade(*grade)
        {
            visit(Var::Grade(v))?;
        }
        if let Some(var) = self.var_of(term) {
            visit(var)?;
            let first = match var {
                Var::Ty(v) => seen.tys.insert(v),
                Var::Comp(v) => seen.comps.insert(v),
                Var::Row(_) | Var::Grade(_) => true,
            };
            if !first {
                return ControlFlow::Continue(());
            }
        }
        self.successors(term)
            .try_for_each(|next| self.reach(next, seen, visit))
    }

    /// Canonical comp-var root under union-find, for the cycle-aware traversals
    /// in `generalize.rs`.
    pub(crate) fn comp_root(&self, v: CompTyVar) -> CompTyVar {
        self.ctys.root(v)
    }

    /// Canonical ty-var root under union-find.  Mirror of `comp_root`.
    pub(crate) fn ty_root(&self, v: TyVar) -> TyVar {
        self.tys.root(v)
    }

    /// A fresh comp-var slot, as a root.  Instantiation mints one per cyclic
    /// comp-var so each use of a recursive scheme gets independent slots.
    pub(crate) fn fresh_comp_root(&mut self) -> CompTyVar {
        self.ctys.fresh()
    }

    /// Mirror of `fresh_comp_root` for cyclic ty bindings.
    pub(crate) fn fresh_ty_root(&mut self) -> TyVar {
        self.tys.fresh()
    }

    /// Pairs with `fresh_comp_root`: the scheme's snapshot, substituted.
    pub(crate) fn bind_comp_root(&mut self, root: CompTyVar, value: CompTy) {
        self.ctys.bind(root, value);
    }

    /// Mirror of `bind_comp_root` for cyclic ty bindings.
    pub(crate) fn bind_ty_root(&mut self, root: TyVar, value: Ty) {
        self.tys.bind(root, value);
    }

    /// The root's binding with substitutions applied, or `None` if unbound or a
    /// bare `Var`; `generalize` snapshots cyclic bindings this way.  Quote *from
    /// the root*, never the stored body: one level below the anchor unrolls the
    /// cycle before the back-edge fires, so the snapshot comes out off by a
    /// level and leaks the original union-find slot there.
    pub(crate) fn resolved_comp_root_binding(&self, root: CompTyVar) -> Option<CompTy> {
        match self.ctys.get_ref(root) {
            Some(CompTy::Var(_)) | None => None,
            Some(_) => Some(self.apply_comp_ty(&CompTy::Var(root))),
        }
    }

    /// Mirror of `resolved_comp_root_binding`; the same anchor-quoting applies.
    pub(crate) fn resolved_ty_root_binding(&self, root: TyVar) -> Option<Ty> {
        match self.tys.get_ref(root) {
            Some(Ty::Var(_)) | None => None,
            Some(_) => Some(self.apply_ty(&Ty::Var(root))),
        }
    }

    /// The comp-var and ty-var roots on some cycle reachable from `ty`.  Reads
    /// the traversal's tags, not its output: a mid-cycle root is tagged on
    /// detection but need not surface as a back-edge.  One walk populates both
    /// tag sets, so `generalize` asks once and reads both.
    pub(super) fn cyclic_roots_in_ty(&self, ty: &Ty) -> (Vec<CompTyVar>, Vec<TyVar>) {
        let mut visited = Visited::default();
        let _applied = self.apply_ty_inner(ty, &mut visited);
        let mut comps: Vec<CompTyVar> = visited.cyclic_comps.into_iter().collect();
        let mut tys: Vec<TyVar> = visited.cyclic_tys.into_iter().collect();
        comps.sort_unstable();
        tys.sort_unstable();
        (comps, tys)
    }

    pub(crate) fn head_ty<'a>(&'a self, ty: &'a Ty) -> Cow<'a, Ty> {
        self.tys.resolve(ty)
    }

    pub(crate) fn head_comp_ty<'a>(&'a self, cty: &'a CompTy) -> Cow<'a, CompTy> {
        self.ctys.resolve(cty)
    }

    /// Canonicalize the head; variables nested in the result stay unresolved.
    pub(crate) fn head_row<'a>(&'a self, row: &'a Row) -> Cow<'a, Row> {
        self.rows.resolve(row)
    }

    pub(crate) fn resolve_ty(&self, ty: &Ty) -> Ty {
        self.head_ty(ty).into_owned()
    }

    /// [`Self::head_comp_ty`], owned, with a `Return`'s grade resolved too.
    pub(crate) fn resolve_comp_ty(&self, cty: &CompTy) -> CompTy {
        match self.head_comp_ty(cty).into_owned() {
            CompTy::Return(grade, ty) => CompTy::Return(self.resolve_grade(grade), ty),
            other => other,
        }
    }

    pub(crate) fn resolve_row(&self, row: &Row) -> Row {
        self.head_row(row).into_owned()
    }

    /// Every `Extend` label, unsorted, and the terminal: `Some(v)` for an open
    /// row, `None` for one closed by `Empty`.  The loop needs no cycle guard:
    /// the occurs check rejects a cyclic row binding before it is installed.
    fn row_spine(&self, row: &Row) -> (Vec<Label>, Option<RowVar>) {
        let mut labels = Vec::new();
        let mut cur = self.resolve_row(row);
        loop {
            match cur {
                Row::Extend(l, _, rest) => {
                    labels.push(l);
                    cur = self.resolve_row(&rest);
                }
                Row::Var(v) => return (labels, Some(v)),
                Row::Empty => return (labels, None),
            }
        }
    }

    pub(crate) fn apply_ty(&self, ty: &Ty) -> Ty {
        let mut visited = Visited::default();
        self.apply_ty_inner(ty, &mut visited)
    }

    /// [`Self::apply_ty`], except that a weak variable stays a variable: a
    /// scheme or binding that mentions one keeps mentioning it after it is fixed.
    pub(crate) fn apply_ty_keeping_weak(&self, ty: &Ty) -> Ty {
        let mut visited = Visited {
            keep_weak: true,
            ..Visited::default()
        };
        self.apply_ty_inner(ty, &mut visited)
    }

    pub(crate) fn apply_comp_ty(&self, cty: &CompTy) -> CompTy {
        let mut visited = Visited::default();
        self.apply_comp_ty_inner(cty, &mut visited)
    }

    pub(crate) fn apply_row(&self, row: &Row) -> Row {
        let mut visited = Visited::default();
        self.apply_row_inner(row, &mut visited)
    }

    pub(super) fn apply_ty_inner(&self, ty: &Ty, visited: &mut Visited) -> Ty {
        // In CBPV every productive recursive value type closes through a
        // `Thunk` — μ values are finite, the recursion lives in the ν
        // computations — so a ty-back-edge onto `Thunk(Var(c))` with `c` on the
        // comp stack redirects to the comp anchor `c`, the canonical capture
        // point: `c` then lands in the scheme's `comp_ty_bindings` and gets a
        // fresh id per instantiation instead of being unrolled and shared.  A
        // cycle truly anchored at a ty-var reaches a `Variant`, not a `Thunk`,
        // and takes the plain fallback.
        let root = match ty {
            Ty::Var(v) => Some(self.tys.root(*v)),
            _ => None,
        };
        if let Some(r) = root {
            if visited.cyclic_tys.contains(&r) {
                return Ty::Var(r);
            }
            if visited.tys.contains(&r) {
                // Match the raw binding, not `resolve_comp_ty` of it: the
                // anchor is `C`'s root, not whatever `C` resolves to now.
                if let Some(Ty::Thunk(b)) = self.tys.get_ref(r)
                    && let CompTy::Var(c) = **b
                {
                    let c_root = self.ctys.root(c);
                    if visited.comps.contains(&c_root) {
                        visited.cyclic_comps.insert(c_root);
                        return Ty::Thunk(Box::new(CompTy::Var(c_root)));
                    }
                }
                visited.cyclic_tys.insert(r);
                return Ty::Var(r);
            }
        }
        if visited.keep_weak
            && let Some(r) = root
            && self.tys.is_weak(r)
        {
            return Ty::Var(r);
        }
        let resolved = self.head_ty(ty);
        if matches!(&*resolved, Ty::Var(_)) {
            return resolved.into_owned();
        }
        if let Some(r) = root {
            visited.tys.insert(r);
        }
        let out = match &*resolved {
            Ty::List(a) => Ty::list(self.apply_ty_inner(a, visited)),
            Ty::Map(a) => Ty::map(self.apply_ty_inner(a, visited)),
            Ty::Handle(a) => Ty::Handle(Box::new(self.apply_ty_inner(a, visited))),
            Ty::Record(r) => Ty::Record(self.apply_row_inner(r, visited)),
            Ty::Variant(r) => Ty::Variant(self.apply_row_inner(r, visited)),
            Ty::Thunk(b) => Ty::Thunk(Box::new(self.apply_comp_ty_inner(b, visited))),
            ground @ (Ty::Unit | Ty::Bytes | Ty::Bool | Ty::Int | Ty::Float | Ty::String) => {
                ground.clone()
            }
            // Enumerated so a new constructor fails the build here rather than
            // falling through unsubstituted.
            Ty::Var(_) => unreachable!("unbound var early-returned; resolved is non-Var here"),
        };
        if let Some(r) = root
            && !visited.cyclic_tys.contains(&r)
        {
            visited.tys.remove(&r);
        }
        out
    }

    fn apply_comp_ty_inner(&self, cty: &CompTy, visited: &mut Visited) -> CompTy {
        // The anchor may sit on either side of the ty/comp boundary, but the
        // cycle traverses both, so every root currently expanding belongs to it
        // and must enter its bindings list to be given a fresh id later.
        let root = match cty {
            CompTy::Var(v) => Some(self.ctys.root(*v)),
            _ => None,
        };
        if let Some(r) = root {
            if visited.comps.contains(&r) {
                visited.cyclic_comps.insert(r);
                return CompTy::Var(r);
            }
            if visited.cyclic_comps.contains(&r) {
                return CompTy::Var(r);
            }
        }
        if visited.keep_weak
            && let Some(r) = root
            && self.ctys.is_weak(r)
        {
            return CompTy::Var(r);
        }
        let resolved = self.head_comp_ty(cty);
        if matches!(&*resolved, CompTy::Var(_)) {
            return resolved.into_owned();
        }
        if let Some(r) = root {
            visited.comps.insert(r);
        }
        let out = match &*resolved {
            CompTy::Return(grade, a) => CompTy::Return(
                self.resolve_grade(*grade),
                Box::new(self.apply_ty_inner(a, visited)),
            ),
            CompTy::Fun(a, b) => CompTy::Fun(
                Box::new(self.apply_ty_inner(a, visited)),
                Box::new(self.apply_comp_ty_inner(b, visited)),
            ),
            CompTy::Var(_) => {
                unreachable!("var early-returned above; resolved is non-Var here")
            }
        };
        if let Some(r) = root
            && !visited.cyclic_comps.contains(&r)
        {
            visited.comps.remove(&r);
        }
        out
    }

    fn apply_row_inner(&self, row: &Row, visited: &mut Visited) -> Row {
        if visited.keep_weak
            && let Row::Var(v) = row
            && self.rows.is_weak(*v)
        {
            return Row::Var(self.rows.root(*v));
        }
        match &*self.head_row(row) {
            Row::Empty => Row::Empty,
            Row::Var(v) => Row::Var(*v),
            Row::Extend(l, ty, rest) => {
                let ty2 = self.apply_ty_inner(ty, visited);
                let rest2 = self.apply_row_inner(rest, visited);
                Row::Extend(l.clone(), Box::new(ty2), Box::new(rest2))
            }
        }
    }

    // Rows, unlike types, are inductive: `ρ = {l: τ}` with `ρ` reachable from
    // `τ` denotes an infinite record and is rejected as `RecursiveRow`.  A row
    // variable can hide in a field type as well as along the spine, so the check
    // descends through both, carrying a `Visited` because a field type may
    // legitimately be cyclic.

    fn row_occurs(&self, v: RowVar, row: &Row, depth: u32) -> Result<bool, TypeErrorKind> {
        let mut visited = Visited::default();
        self.row_occurs_inner(v, row, &mut visited, depth)
    }

    fn row_occurs_inner(
        &self,
        v: RowVar,
        row: &Row,
        visited: &mut Visited,
        depth: u32,
    ) -> Result<bool, TypeErrorKind> {
        match &*self.head_row(row) {
            Row::Empty => Ok(false),
            Row::Var(u) => Ok(*u == v),
            // Through the payload, not only along the spine: a spine-only
            // check admits a cycle anchored at no type or computation
            // variable, which nothing here could apply, snapshot or re-anchor.
            Row::Extend(_, ty, rest) => {
                let inside = self.ty_occurs_row(v, ty, visited, deeper(depth)?)?;
                Ok(inside || self.row_occurs_inner(v, rest, visited, depth)?)
            }
        }
    }

    fn ty_occurs_row(
        &self,
        v: RowVar,
        ty: &Ty,
        visited: &mut Visited,
        depth: u32,
    ) -> Result<bool, TypeErrorKind> {
        let root = match ty {
            Ty::Var(v) => Some(self.tys.root(*v)),
            _ => None,
        };
        if let Some(r) = root
            && !visited.tys.insert(r)
        {
            return Ok(false);
        }
        match &*self.head_ty(ty) {
            Ty::List(a) | Ty::Map(a) | Ty::Handle(a) => {
                self.ty_occurs_row(v, a, visited, deeper(depth)?)
            }
            Ty::Record(r) | Ty::Variant(r) => self.row_occurs_inner(v, r, visited, deeper(depth)?),
            Ty::Thunk(c) => self.comp_occurs_row(v, c, visited, deeper(depth)?),
            // Enumerated rather than caught: a future row-embedding
            // constructor skipped here lets a cyclic row install undetected.
            Ty::Var(_) | Ty::Unit | Ty::Bytes | Ty::Bool | Ty::Int | Ty::Float | Ty::String => {
                Ok(false)
            }
        }
    }

    fn comp_occurs_row(
        &self,
        v: RowVar,
        cty: &CompTy,
        visited: &mut Visited,
        depth: u32,
    ) -> Result<bool, TypeErrorKind> {
        let root = match cty {
            CompTy::Var(v) => Some(self.ctys.root(*v)),
            _ => None,
        };
        if let Some(r) = root
            && !visited.comps.insert(r)
        {
            return Ok(false);
        }
        match &*self.head_comp_ty(cty) {
            CompTy::Var(_) => Ok(false),
            CompTy::Return(_, a) => self.ty_occurs_row(v, a, visited, deeper(depth)?),
            CompTy::Fun(a, b) => Ok(self.ty_occurs_row(v, a, visited, deeper(depth)?)?
                || self.comp_occurs_row(v, b, visited, deeper(depth)?)?),
        }
    }

    // A key fingerprints the *given term*, not its equi-recursive expansion, so
    // equal keys mean the same obligation against the same anchor — the fixed
    // point the guard discharges.  Being a structural recursion, it spends the
    // `MAX_UNIFY_DEPTH` budget threaded in from the calling unify arm.

    fn ty_key(&mut self, ty: &Ty, depth: u32) -> Result<TyKey, TypeErrorKind> {
        Ok(match ty {
            Ty::Unit => TyKey::Unit,
            Ty::Bytes => TyKey::Bytes,
            Ty::Bool => TyKey::Bool,
            Ty::Int => TyKey::Int,
            Ty::Float => TyKey::Float,
            Ty::String => TyKey::String,
            Ty::List(a) => TyKey::List(Box::new(self.ty_key(a, deeper(depth)?)?)),
            Ty::Map(a) => TyKey::Map(Box::new(self.ty_key(a, deeper(depth)?)?)),
            Ty::Record(r) => TyKey::Record(self.row_key(r, deeper(depth)?)?),
            Ty::Variant(r) => TyKey::Variant(self.row_key(r, deeper(depth)?)?),
            Ty::Thunk(c) => TyKey::Thunk(Box::new(self.comp_key(c, deeper(depth)?)?)),
            Ty::Handle(a) => TyKey::Handle(Box::new(self.ty_key(a, deeper(depth)?)?)),
            Ty::Var(i) => TyKey::Var(self.tys.find(*i)),
        })
    }

    fn comp_key(&mut self, cty: &CompTy, depth: u32) -> Result<CompTyKey, TypeErrorKind> {
        Ok(match cty {
            CompTy::Return(grade, t) => CompTyKey::Return(
                self.resolve_grade(*grade),
                Box::new(self.ty_key(t, deeper(depth)?)?),
            ),
            CompTy::Fun(a, b) => CompTyKey::Fun(
                Box::new(self.ty_key(a, deeper(depth)?)?),
                Box::new(self.comp_key(b, deeper(depth)?)?),
            ),
            CompTy::Var(i) => CompTyKey::Var(self.ctys.find(*i)),
        })
    }

    fn row_key(&mut self, row: &Row, depth: u32) -> Result<RowKey, TypeErrorKind> {
        Ok(match row {
            Row::Empty => RowKey::Empty,
            Row::Var(i) => RowKey::Var(self.rows.find(*i)),
            Row::Extend(l, ty, rest) => RowKey::Extend(
                l.clone(),
                Box::new(self.ty_key(ty, deeper(depth)?)?),
                Box::new(self.row_key(rest, depth)?),
            ),
        })
    }

    /// Unify value types `a` and `b`, binding variables in place.
    ///
    /// # Errors
    /// [`TypeErrorKind::TyMismatch`] on mismatched structure,
    /// [`TypeErrorKind::RecursiveRow`] from an embedded row, or
    /// [`TypeErrorKind::TypeTooDeep`].
    pub(crate) fn unify_ty(&mut self, a: &Ty, b: &Ty) -> Result<(), TypeErrorKind> {
        let mut pairs = Pairs::default();
        self.unify_ty_inner(a, b, &mut pairs, 0)
    }

    fn unify_ty_inner(
        &mut self,
        a: &Ty,
        b: &Ty,
        pairs: &mut Pairs,
        depth: u32,
    ) -> Result<(), TypeErrorKind> {
        match (a, b) {
            (Ty::Var(ai), Ty::Var(bi)) => {
                let (ar, br) = (self.tys.find(*ai), self.tys.find(*bi));
                if guard_pair(&mut pairs.tys, ar, br) {
                    return Ok(());
                }
            }
            // One-sided, which is how a value-anchored cycle meets its
            // comp-anchored twin: the two never present as `Var`/`Var`.
            (Ty::Var(vi), other) | (other, Ty::Var(vi)) => {
                let (root, key) = (self.tys.find(*vi), self.ty_key(other, depth)?);
                if guard_expansion(&mut pairs.ty_expansions, root, key) {
                    return Ok(());
                }
            }
            _ => {}
        }

        let (a_weak, b_weak) = (self.weak_var(a), self.weak_var(b));
        let a = self.resolve_ty(a);
        let b = self.resolve_ty(b);

        if let (Ty::Var(ai), Ty::Var(bi)) = (&a, &b) {
            return self.unite_tys(*ai, *bi);
        }
        // Unified with a weak variable, even one already fixed, is weak.
        if let Ty::Var(vi) = &a {
            if let Some(source) = b_weak {
                self.tys.mark_weak(*vi, source);
            }
            return self.bind_ty(*vi, b, depth);
        }
        if let Ty::Var(vi) = &b {
            if let Some(source) = a_weak {
                self.tys.mark_weak(*vi, source);
            }
            return self.bind_ty(*vi, a, depth);
        }
        let depth = deeper(depth)?;
        match (a, b) {
            (Ty::Unit, Ty::Unit)
            | (Ty::Bool, Ty::Bool)
            | (Ty::Int, Ty::Int)
            | (Ty::Float, Ty::Float)
            | (Ty::String, Ty::String)
            | (Ty::Bytes, Ty::Bytes) => Ok(()),
            (Ty::List(a1), Ty::List(b1))
            | (Ty::Map(a1), Ty::Map(b1))
            | (Ty::Handle(a1), Ty::Handle(b1)) => self.unify_ty_inner(&a1, &b1, pairs, depth),
            (Ty::Record(r1), Ty::Record(r2)) | (Ty::Variant(r1), Ty::Variant(r2)) => {
                let r = self.unify_row_inner(&r1, &r2, pairs, depth);
                self.name_alternatives(&r1, &r2, r)
            }
            (Ty::Thunk(a1), Ty::Thunk(b1)) => self.unify_comp_ty_inner(&a1, &b1, pairs, depth),
            // Enumerated, not `_`, here and in the two matches below: a new
            // constructor then fails the build until it is routed above,
            // instead of being reported as a mismatch with itself.
            (
                a @ (Ty::Unit
                | Ty::Bytes
                | Ty::Bool
                | Ty::Int
                | Ty::Float
                | Ty::String
                | Ty::List(_)
                | Ty::Map(_)
                | Ty::Record(_)
                | Ty::Variant(_)
                | Ty::Thunk(_)
                | Ty::Handle(_)
                | Ty::Var(_)),
                b,
            ) => Err(TypeErrorKind::TyMismatch {
                expected: Box::new(a),
                actual: Box::new(b),
            }),
        }
    }

    /// Bind `v`'s root to the structure `val`, unless `val` reaches it without
    /// crossing data.  Var–var unions cannot close a cycle, so binding is the
    /// only place one can form.
    fn bind_ty(&mut self, v: TyVar, val: Ty, depth: u32) -> Result<(), TypeErrorKind> {
        let root = self.tys.find(v);
        self.refuse_data_free_cycle(Anchor::Ty(root), Reach::Ty(&val), depth)?;
        self.admit(root, &val, depth)?;
        if let Some(source) = self.tys.weak_source(root).cloned() {
            self.mark_weak(&val, &source);
        }
        if let Some(at) = self.at {
            self.bound_at.entry(root).or_insert(at);
        }
        self.tys.bind(root, val);
        Ok(())
    }

    /// A weak row variable bound to a spine: what the spine mentions is weak too.
    fn weaken_row(&mut self, root: RowVar, spine: &Row) {
        if let Some(source) = self.rows.weak_source(root).cloned() {
            self.mark_weak(&Ty::Record(spine.clone()), &source);
        }
    }

    fn bind_comp_ty(&mut self, v: CompTyVar, val: CompTy, depth: u32) -> Result<(), TypeErrorKind> {
        let root = self.ctys.find(v);
        self.refuse_data_free_cycle(Anchor::Comp(root), Reach::Comp(&val), depth)?;
        if let Some(source) = self.ctys.weak_source(root).cloned() {
            self.mark_weak(&Ty::Thunk(Box::new(val.clone())), &source);
        }
        self.ctys.bind(root, val);
        Ok(())
    }

    fn refuse_data_free_cycle(
        &self,
        anchor: Anchor,
        from: Reach<'_>,
        depth: u32,
    ) -> Result<(), TypeErrorKind> {
        let mut visited = Visited::default();
        let via = match from {
            Reach::Ty(ty) => self.ty_reaches(anchor, ty, CycleVia::Returns, &mut visited, depth)?,
            Reach::Comp(cty) => {
                self.comp_reaches(anchor, cty, CycleVia::Returns, &mut visited, depth)?
            }
        };
        via.map_or(Ok(()), |via| Err(TypeErrorKind::CyclicType { via }))
    }

    /// Whether `anchor` is reachable from `ty` along `U`, `→`, `F` and
    /// `Handle` alone, and by which edge.  Data edges are not crossed, so the
    /// search is finite over the acyclic non-data graph and a visited set keyed
    /// by root is sound.
    fn ty_reaches(
        &self,
        anchor: Anchor,
        ty: &Ty,
        via: CycleVia,
        visited: &mut Visited,
        depth: u32,
    ) -> Result<Option<CycleVia>, TypeErrorKind> {
        if let Ty::Var(i) = ty {
            let root = self.tys.root(*i);
            if anchor == Anchor::Ty(root) {
                return Ok(Some(via));
            }
            if !visited.tys.insert(root) {
                return Ok(None);
            }
        }
        match &*self.head_ty(ty) {
            Ty::Thunk(c) => self.comp_reaches(anchor, c, via, visited, deeper(depth)?),
            Ty::Handle(a) => self.ty_reaches(anchor, a, via, visited, deeper(depth)?),
            Ty::List(_) | Ty::Map(_) | Ty::Record(_) | Ty::Variant(_) => Ok(None),
            Ty::Var(_) | Ty::Unit | Ty::Bytes | Ty::Bool | Ty::Int | Ty::Float | Ty::String => {
                Ok(None)
            }
        }
    }

    fn comp_reaches(
        &self,
        anchor: Anchor,
        cty: &CompTy,
        via: CycleVia,
        visited: &mut Visited,
        depth: u32,
    ) -> Result<Option<CycleVia>, TypeErrorKind> {
        if let CompTy::Var(i) = cty {
            let root = self.ctys.root(*i);
            if anchor == Anchor::Comp(root) {
                return Ok(Some(via));
            }
            if !visited.comps.insert(root) {
                return Ok(None);
            }
        }
        let depth = deeper(depth)?;
        match &*self.head_comp_ty(cty) {
            CompTy::Var(_) => Ok(None),
            CompTy::Return(_, a) => self.ty_reaches(anchor, a, CycleVia::Returns, visited, depth),
            CompTy::Fun(a, b) => {
                let arg_via = if matches!(a.as_ref(), Ty::Var(_)) {
                    CycleVia::Applied
                } else {
                    CycleVia::Argument
                };
                match self.ty_reaches(anchor, a, arg_via, visited, depth)? {
                    Some(via) => Ok(Some(via)),
                    None => self.comp_reaches(anchor, b, CycleVia::Returns, visited, depth),
                }
            }
        }
    }

    /// Row unification using the Rémy rewrite rule.
    ///
    /// # Errors
    /// [`TypeErrorKind::RowMissingField`] /
    /// [`RowExtraField`](TypeErrorKind::RowExtraField) when a closed row lacks
    /// or carries a label, [`TypeErrorKind::TyMismatch`] on a clashing shared
    /// label or mixed alphabets, [`TypeErrorKind::RecursiveRow`] when there is
    /// no solution, or [`TypeErrorKind::TypeTooDeep`].
    pub(crate) fn unify_row(&mut self, a: &Row, b: &Row) -> Result<(), TypeErrorKind> {
        let mut pairs = Pairs::default();
        let r = self.unify_row_inner(a, b, &mut pairs, 0);
        self.name_alternatives(a, b, r)
    }

    /// Fill in a rejected read's alternatives from the rows as they stood on
    /// entry.  The Rémy rewrite peels labels off as it searches, so by the time
    /// a closed row runs out there is nothing left to enumerate — only a frame
    /// still holding both original rows can say what was on offer.
    ///
    /// Which row to name is decided, not assumed: the record that rejected the
    /// label is the one whose own labels lack it, and either side can be that
    /// record.
    fn name_alternatives(
        &self,
        a: &Row,
        b: &Row,
        result: Result<(), TypeErrorKind>,
    ) -> Result<(), TypeErrorKind> {
        let Err(TypeErrorKind::RowExtraField { label, known }) = result else {
            return result;
        };
        if !known.is_empty() {
            return Err(TypeErrorKind::RowExtraField { label, known });
        }
        let names = |ls: Vec<Label>| ls.iter().map(Label::to_string).collect::<Vec<_>>();
        let a_names = names(self.row_spine(a).0);
        let known = if a_names.contains(&label) {
            names(self.row_spine(b).0)
        } else {
            a_names
        };
        Err(TypeErrorKind::RowExtraField { label, known })
    }

    /// `depth` is where this row sits, set by the record/variant arm that
    /// descended into it.  The spine is width — the matched-label case iterates
    /// in this `loop` and the Rémy re-entries pass `depth` through — so only
    /// stepping into a field *type* charges a level.
    fn unify_row_inner(
        &mut self,
        a: &Row,
        b: &Row,
        pairs: &mut Pairs,
        depth: u32,
    ) -> Result<(), TypeErrorKind> {
        let mut entry_weak = (self.weak_row_var(a), self.weak_row_var(b));
        let mut a = self.resolve_row(a);
        let mut b = self.resolve_row(b);
        loop {
            // Only the rows as given can be weak variables fixed to a spine.
            let (a_weak, b_weak) = std::mem::take(&mut entry_weak);
            if let (Row::Var(ai), Row::Var(bi)) = (&a, &b) {
                self.unite_rows(*ai, *bi);
                return Ok(());
            }
            if let Row::Var(vi) = &a {
                let vi = *vi;
                if let Some(source) = b_weak {
                    self.rows.mark_weak(vi, source);
                }
                if self.row_occurs(vi, &b, depth)? {
                    return Err(TypeErrorKind::RecursiveRow);
                }
                let r = self.rows.find(vi);
                self.admit_row(r, &b, depth)?;
                self.weaken_row(r, &b);
                self.rows.bind(r, b);
                return Ok(());
            }
            if let Row::Var(vi) = &b {
                let vi = *vi;
                if let Some(source) = a_weak {
                    self.rows.mark_weak(vi, source);
                }
                if self.row_occurs(vi, &a, depth)? {
                    return Err(TypeErrorKind::RecursiveRow);
                }
                let r = self.rows.find(vi);
                self.admit_row(r, &a, depth)?;
                self.weaken_row(r, &a);
                self.rows.bind(r, a);
                return Ok(());
            }

            match (a, b) {
                (Row::Empty, Row::Empty) => return Ok(()),
                (Row::Empty, Row::Extend(l, ..)) => {
                    return Err(TypeErrorKind::RowExtraField {
                        label: l.to_string(),
                        known: Vec::new(),
                    });
                }
                (Row::Extend(l, ..), Row::Empty) => {
                    return Err(TypeErrorKind::RowMissingField {
                        label: l.to_string(),
                    });
                }
                (Row::Extend(l1, t1, r1), Row::Extend(l2, t2, r2)) => {
                    if l1 == l2 {
                        self.unify_ty_inner(&t1, &t2, pairs, deeper(depth)?)?;
                        // In place, not a deeper frame: a wide row is O(1) stack.
                        a = self.resolve_row(&r1);
                        b = self.resolve_row(&r2);
                        continue;
                    }
                    // A field name and a constructor are different labels
                    // however they are spelled, so a row carrying both
                    // typechecks against neither pure form.
                    if std::mem::discriminant(&l1) != std::mem::discriminant(&l2) {
                        return Err(TypeErrorKind::TyMismatch {
                            expected: Box::new(Ty::Record(Row::Extend(
                                l1,
                                t1,
                                Box::new(Row::Empty),
                            ))),
                            actual: Box::new(Ty::Record(Row::Extend(l2, t2, Box::new(Row::Empty)))),
                        });
                    }
                    // Scoped-labels side condition (Gaster–Jones, Leijen): two
                    // rows on the *same* tail with different label multisets
                    // have no finite or rational solution — the rewrite would
                    // re-enter with the mismatch intact, minting a fresh tail
                    // each turn.  The disagreement can sit below the head, so
                    // compare whole spines; a permutation does have a solution
                    // and must still take it.
                    let (mut left, left_tail) = self.row_spine(&r1);
                    let (mut right, right_tail) = self.row_spine(&r2);
                    left.push(l1.clone());
                    right.push(l2.clone());
                    if let (Some(lt), Some(rt)) = (left_tail, right_tail)
                        && lt == rt
                    {
                        left.sort();
                        right.sort();
                        if left != right {
                            return Err(TypeErrorKind::RecursiveRow);
                        }
                    }
                    let rho = self.fresh_row_var();
                    let new_r1 = Row::Extend(l2, t2, Box::new(Row::Var(rho)));
                    let new_r2 = Row::Extend(l1, t1, Box::new(Row::Var(rho)));
                    self.unify_row_inner(&r1, &new_r1, pairs, depth)?;
                    return self.unify_row_inner(&new_r2, &r2, pairs, depth);
                }
                (Row::Var(_), _) | (_, Row::Var(_)) => {
                    unreachable!("Row::Var pairs are handled by the early-return blocks above")
                }
            }
        }
    }

    /// Unify computation types `a` and `b`, binding variables in place.
    ///
    /// # Errors
    /// [`TypeErrorKind::CompTyMismatch`] on mismatched structure or disagreeing
    /// modes or return types, [`TypeErrorKind::TypeTooDeep`] past the budget.
    pub(crate) fn unify_comp_ty(&mut self, a: &CompTy, b: &CompTy) -> Result<(), TypeErrorKind> {
        let mut pairs = Pairs::default();
        self.unify_comp_ty_inner(a, b, &mut pairs, 0)
    }

    fn unify_comp_ty_inner(
        &mut self,
        a: &CompTy,
        b: &CompTy,
        pairs: &mut Pairs,
        depth: u32,
    ) -> Result<(), TypeErrorKind> {
        match (a, b) {
            (CompTy::Var(ai), CompTy::Var(bi)) => {
                let (ar, br) = (self.ctys.find(*ai), self.ctys.find(*bi));
                if guard_pair(&mut pairs.comps, ar, br) {
                    return Ok(());
                }
            }
            // One-sided: the comp half of the anchoring mismatch, `F T ~= C`.
            (CompTy::Var(vi), other) | (other, CompTy::Var(vi)) => {
                let (root, key) = (self.ctys.find(*vi), self.comp_key(other, depth)?);
                if guard_expansion(&mut pairs.comp_expansions, root, key) {
                    return Ok(());
                }
            }
            _ => {}
        }

        let (a_weak, b_weak) = (self.weak_comp_var(a), self.weak_comp_var(b));
        let a = self.resolve_comp_ty(a);
        let b = self.resolve_comp_ty(b);

        if let (CompTy::Var(ai), CompTy::Var(bi)) = (&a, &b) {
            self.ctys.unite(*ai, *bi);
            return Ok(());
        }
        if let CompTy::Var(vi) = &a {
            if let Some(source) = b_weak {
                self.ctys.mark_weak(*vi, source);
            }
            return self.bind_comp_ty(*vi, b, depth);
        }
        if let CompTy::Var(vi) = &b {
            if let Some(source) = a_weak {
                self.ctys.mark_weak(*vi, source);
            }
            return self.bind_comp_ty(*vi, a, depth);
        }
        let depth = deeper(depth)?;
        match (a, b) {
            (CompTy::Return(ga, ta), CompTy::Return(gb, tb)) => {
                let mismatch = |u: &Self| TypeErrorKind::CompTyMismatch {
                    expected: u.apply_comp_ty(&CompTy::Return(ga, ta.clone())),
                    actual: u.apply_comp_ty(&CompTy::Return(gb, tb.clone())),
                };
                if !self.unify_grade(ga, gb) {
                    return Err(mismatch(self));
                }
                // A spent depth budget is exhaustion rather than disagreement, and
                // a kind refusal is not a disagreement between two types at all,
                // so both propagate verbatim.
                match self.unify_ty_inner(&ta, &tb, pairs, depth) {
                    Ok(()) => Ok(()),
                    Err(
                        e @ (TypeErrorKind::TypeTooDeep
                        | TypeErrorKind::CyclicType { .. }
                        | TypeErrorKind::KindMismatch { .. }),
                    ) => Err(e),
                    Err(_) => Err(mismatch(self)),
                }
            }
            (CompTy::Fun(a1, b1), CompTy::Fun(a2, b2)) => {
                self.unify_ty_inner(&a1, &a2, pairs, depth)?;
                self.unify_comp_ty_inner(&b1, &b2, pairs, depth)
            }
            (a @ (CompTy::Return(..) | CompTy::Fun(..) | CompTy::Var(_)), b) => {
                Err(TypeErrorKind::CompTyMismatch {
                    expected: a,
                    actual: b,
                })
            }
        }
    }
}

impl Default for Unifier {
    fn default() -> Self {
        Self::new()
    }
}

/// Order a root pair so `(a, b)` and `(b, a)` key the same guard entry.
fn ordered_pair<V: Ord>(a: V, b: V) -> (V, V) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Symmetric obligation: two var roots.  `true` when in progress or coincident.
fn guard_pair<V: Ord + std::hash::Hash>(seen: &mut HashSet<(V, V)>, a: V, b: V) -> bool {
    a == b || !seen.insert(ordered_pair(a, b))
}

/// One-sided obligation: a var root against a key.  `true` when in progress.
fn guard_expansion<V: Eq + std::hash::Hash, K: Eq + std::hash::Hash>(
    seen: &mut HashSet<(V, K)>,
    root: V,
    key: K,
) -> bool {
    !seen.insert((root, key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::FileId;

    /// ``Variant{`more: {head: String, tail: Thunk(tail)}, `done: Unit}``.
    fn step(tail: CompTy) -> Ty {
        let payload = Ty::Record(Row::Extend(
            Label::Field("head".into()),
            Box::new(Ty::String),
            Box::new(Row::Extend(
                Label::Field("tail".into()),
                Box::new(Ty::Thunk(Box::new(tail))),
                Box::new(Row::Empty),
            )),
        ));
        Ty::Variant(Row::Extend(
            Label::Case("more".into()),
            Box::new(payload),
            Box::new(Row::Extend(
                Label::Case("done".into()),
                Box::new(Ty::Unit),
                Box::new(Row::Empty),
            )),
        ))
    }

    /// `T = Step(F T)` and `C = F Step(C)` are one equi-recursive type under two
    /// anchors, so `T ≐ Step(C)` must terminate.  It re-enters as
    /// `Var`-vs-structure throughout, which a `Var`/`Var`-only guard misses.
    #[test]
    fn unifies_value_anchored_and_comp_anchored_stream() {
        let mut u = Unifier::new();

        let t = u.fresh_tyvar();
        let t_root = u.ty_root(t);
        let t_body = step(CompTy::pure(Ty::Var(t)));
        u.bind_ty_root(t_root, t_body);

        let CompTy::Var(c_root) = u.fresh_comp_ty() else {
            unreachable!("fresh_comp_ty yields a Var")
        };
        let step_c = step(CompTy::Var(c_root));
        u.bind_comp_root(c_root, CompTy::pure(step_c.clone()));

        u.unify_ty(&Ty::Var(t), &step_c)
            .expect("equi-recursive stream types unify regardless of anchor");
    }

    /// `α ≐ {α → Returns Unit}`: a function taking itself.
    #[test]
    fn a_cycle_through_arrows_alone_is_refused() {
        let mut u = Unifier::new();
        let t = u.fresh_tyvar();
        let body = Ty::Thunk(Box::new(CompTy::Fun(
            Box::new(Ty::Var(t)),
            Box::new(CompTy::pure(Ty::Unit)),
        )));
        let err = u
            .unify_ty(&Ty::Var(t), &body)
            .expect_err("a type reaching itself through U and → alone must not bind");
        assert!(
            matches!(
                err,
                TypeErrorKind::CyclicType {
                    via: CycleVia::Applied
                }
            ),
            "expected CyclicType, got {err:?}"
        );
    }

    /// `γ ≐ Returns {γ}`: a computation returning a thunk of itself.
    #[test]
    fn a_computation_returning_itself_is_refused() {
        let mut u = Unifier::new();
        let c = u.fresh_comp_ty();
        let body = CompTy::pure(Ty::Thunk(Box::new(c.clone())));
        let err = u
            .unify_comp_ty(&c, &body)
            .expect_err("a computation returning itself must not bind");
        assert!(
            matches!(
                err,
                TypeErrorKind::CyclicType {
                    via: CycleVia::Returns
                }
            ),
            "expected CyclicType, got {err:?}"
        );
    }

    /// `α ≐ [{α → Returns Unit}]`: the same cycle, but through a list.
    #[test]
    fn a_cycle_through_data_is_accepted() {
        let mut u = Unifier::new();
        let t = u.fresh_tyvar();
        let body = Ty::list(Ty::Thunk(Box::new(CompTy::Fun(
            Box::new(Ty::Var(t)),
            Box::new(CompTy::pure(Ty::Unit)),
        ))));
        u.unify_ty(&Ty::Var(t), &body)
            .expect("a cycle through a list is a legitimate recursive type");
    }

    /// A `List(List(… Int …))` spine: no variable, so nothing to memoize.
    fn deep_list(n: u32) -> Ty {
        let mut ty = Ty::Int;
        for _ in 0..n {
            ty = Ty::list(ty);
        }
        ty
    }

    /// A `Ty` deep enough to trip the bound is also deep enough that building
    /// and recursively `Drop`ping the `Box` chain — nothing to do with the
    /// unifier — nears the default 2 MiB test-thread ceiling.
    fn on_deep_stack(body: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(body)
            .expect("spawn deep-stack thread")
            .join()
            .expect("deep-stack assertion");
    }

    /// A deep variable-free type against its twin drives the `List` arm over.
    #[test]
    fn deeply_nested_type_is_too_deep_not_a_stack_overflow() {
        on_deep_stack(|| {
            let mut u = Unifier::new();
            let a = deep_list(MAX_UNIFY_DEPTH + 100);
            let b = deep_list(MAX_UNIFY_DEPTH + 100);
            let err = u
                .unify_ty(&a, &b)
                .expect_err("a type nested past the ceiling must not unify");
            assert!(
                matches!(err, TypeErrorKind::TypeTooDeep),
                "expected TypeTooDeep, got {err:?}"
            );
        });
    }

    /// A ty-var against a deep concrete type fingerprints it through `ty_key`,
    /// a recursion *outside* the unify arms that shares the same budget.
    #[test]
    fn deeply_nested_ty_key_is_too_deep_not_a_stack_overflow() {
        on_deep_stack(|| {
            let mut u = Unifier::new();
            let v = u.fresh_ty();
            let deep = deep_list(MAX_UNIFY_DEPTH + 100);
            let err = u
                .unify_ty(&v, &deep)
                .expect_err("the key fingerprint of a too-deep type must report TypeTooDeep");
            assert!(
                matches!(err, TypeErrorKind::TypeTooDeep),
                "expected TypeTooDeep, got {err:?}"
            );
        });
    }

    /// Binding a row variable runs the occurs check over each field *type* —
    /// the third structural recursion outside the unify arms, bounded alike.
    #[test]
    fn deeply_nested_field_type_in_occurs_is_too_deep() {
        on_deep_stack(|| {
            let mut u = Unifier::new();
            let rho = u.fresh_row_var();
            let row = Row::Extend(
                Label::Field("x".into()),
                Box::new(deep_list(MAX_UNIFY_DEPTH + 100)),
                Box::new(Row::Empty),
            );
            let err = u.unify_row(&Row::Var(rho), &row).expect_err(
                "a too-deep field type must report TypeTooDeep through the occurs check",
            );
            assert!(
                matches!(err, TypeErrorKind::TypeTooDeep),
                "expected TypeTooDeep, got {err:?}"
            );
        });
    }

    fn record_row(fields: &[(&str, Ty)]) -> Row {
        fields.iter().rev().fold(Row::Empty, |rest, (l, t)| {
            Row::Extend(
                Label::Field((*l).into()),
                Box::new(t.clone()),
                Box::new(rest),
            )
        })
    }

    fn open_row(fields: &[(&str, Ty)], tail: RowVar) -> Row {
        fields.iter().rev().fold(Row::Var(tail), |rest, (l, t)| {
            Row::Extend(
                Label::Field((*l).into()),
                Box::new(t.clone()),
                Box::new(rest),
            )
        })
    }

    fn resolved_fields(u: &Unifier, row: &Row) -> std::collections::HashMap<String, Ty> {
        let mut out = std::collections::HashMap::new();
        let mut cur = u.apply_row(row);
        loop {
            match cur {
                Row::Extend(l, t, rest) => {
                    out.insert(l.to_string(), *t);
                    cur = *rest;
                }
                _ => return out,
            }
        }
    }

    /// The no-false-positive half of the bound: a record far wider than
    /// [`MAX_UNIFY_DEPTH`] but shallow must still unify.
    #[test]
    fn wide_record_unifies_without_being_too_deep() {
        let width = (MAX_UNIFY_DEPTH as usize) * 4;
        let labels: Vec<String> = (0..width).map(|i| format!("f{i}")).collect();
        let fields: Vec<(&str, Ty)> = labels.iter().map(|l| (l.as_str(), Ty::Int)).collect();
        let mut u = Unifier::new();
        let left = record_row(&fields);
        let right = record_row(&fields);
        u.unify_row(&left, &right)
            .expect("a wide-but-shallow record must unify, not be rejected as too deep");
    }

    /// The same width property for a variant row: alternatives are siblings.
    #[test]
    fn wide_variant_unifies_without_being_too_deep() {
        let width = (MAX_UNIFY_DEPTH as usize) * 4;
        let labels: Vec<String> = (0..width).map(|i| format!("`t{i}")).collect();
        let fields: Vec<(&str, Ty)> = labels.iter().map(|l| (l.as_str(), Ty::Unit)).collect();
        let mut u = Unifier::new();
        let left = Ty::Variant(record_row(&fields));
        let right = Ty::Variant(record_row(&fields));
        u.unify_ty(&left, &right)
            .expect("a wide-but-shallow variant must unify, not be rejected as too deep");
    }

    /// The rewrite pairs labels regardless of spine position: order is free.
    #[test]
    fn remy_rewrite_under_permutation() {
        let mut u = Unifier::new();
        let a = u.fresh_ty();
        let b = u.fresh_ty();
        let left = record_row(&[("a", Ty::Int), ("b", Ty::String)]);
        let right = record_row(&[("b", b.clone()), ("a", a.clone())]);
        u.unify_row(&left, &right).expect("permuted records unify");
        assert_eq!(
            u.apply_ty(&a),
            Ty::Int,
            "label `a` matched across positions"
        );
        assert_eq!(
            u.apply_ty(&b),
            Ty::String,
            "label `b` matched across positions"
        );
    }

    /// An open row against a closed one binds the tail to carry the surplus.
    #[test]
    fn remy_rewrite_binds_open_tail() {
        let mut u = Unifier::new();
        let rho = u.fresh_row_var();
        let open = open_row(&[("a", Ty::Int)], rho);
        let closed = record_row(&[("a", Ty::Int), ("b", Ty::String)]);
        u.unify_row(&open, &closed)
            .expect("open row absorbs the extra field");
        let fields = resolved_fields(&u, &Row::Var(rho));
        assert_eq!(
            fields.get("b"),
            Some(&Ty::String),
            "tail absorbed the `b` field"
        );
    }

    /// Which record lacks the label must not depend on the order a literal
    /// wrote its fields in: the Rémy rewrite keeps each side on its side.
    #[test]
    fn a_missing_field_is_reported_whatever_the_label_order() {
        let closed = |fields: &[&str]| {
            fields.iter().rev().fold(Row::Empty, |rest, l| {
                Row::Extend(Label::Field((*l).into()), Box::new(Ty::Int), Box::new(rest))
            })
        };
        let wanted = closed(&["p", "q", "r"]);
        for written in [["p", "q"], ["q", "p"]] {
            let err = Unifier::new()
                .unify_row(&closed(&written), &wanted)
                .expect_err("a record without `r` must not unify");
            assert!(
                matches!(&err, TypeErrorKind::RowExtraField { label, .. } if label == "r"),
                "{written:?}: expected RowExtraField(r), got {err:?}"
            );
        }
    }

    /// `{x: Int | ρ} ≐ {y: Int | ρ}` has no solution; report, do not diverge.
    #[test]
    fn shared_tail_mismatched_heads_is_recursive_row() {
        let mut u = Unifier::new();
        let rho = u.fresh_row_var();
        let left = open_row(&[("x", Ty::Int)], rho);
        let right = open_row(&[("y", Ty::Int)], rho);
        let err = u
            .unify_row(&left, &right)
            .expect_err("shared-tail mismatched heads must not unify");
        assert!(
            matches!(err, TypeErrorKind::RecursiveRow),
            "expected RecursiveRow, got {err:?}"
        );
    }

    /// The same divergence *below* the head: the immediate rests are `Extend`
    /// nodes, not the shared variable, so only a whole-spine comparison sees it.
    #[test]
    fn shared_tail_mismatched_heads_deep_is_recursive_row() {
        let mut u = Unifier::new();
        let rho = u.fresh_row_var();
        let left = open_row(&[("x", Ty::Int), ("a", Ty::Int)], rho);
        let right = open_row(&[("y", Ty::Int), ("b", Ty::Int)], rho);
        let err = u
            .unify_row(&left, &right)
            .expect_err("shared-tail deep mismatch must not unify");
        assert!(
            matches!(err, TypeErrorKind::RecursiveRow),
            "expected RecursiveRow, got {err:?}"
        );
    }

    /// A shared tail with the *same* multiset reordered is a permutation, not a
    /// divergence: it has a solution, and the guard must let the rewrite run.
    #[test]
    fn shared_tail_permutation_still_unifies() {
        let mut u = Unifier::new();
        let rho = u.fresh_row_var();
        let a = u.fresh_ty();
        let b = u.fresh_ty();
        let left = open_row(&[("a", Ty::Int), ("b", Ty::String)], rho);
        let right = open_row(&[("b", b.clone()), ("a", a.clone())], rho);
        u.unify_row(&left, &right)
            .expect("shared-tail permuted rows unify");
        assert_eq!(
            u.apply_ty(&a),
            Ty::Int,
            "label `a` matched across positions"
        );
        assert_eq!(
            u.apply_ty(&b),
            Ty::String,
            "label `b` matched across positions"
        );
    }

    /// `ρ ≐ {x: {n: Int | ρ}}` denotes an infinite record, so the occurs check
    /// must reach it by descending into the field type.
    #[test]
    fn cycle_through_field_type_is_recursive_row() {
        let mut u = Unifier::new();
        let rho = u.fresh_row_var();
        let inner = Ty::Record(open_row(&[("n", Ty::Int)], rho));
        let outer = record_row(&[("x", inner)]);
        let err = u
            .unify_row(&Row::Var(rho), &outer)
            .expect_err("a row reachable from its own field type must not unify");
        assert!(
            matches!(err, TypeErrorKind::RecursiveRow),
            "expected RecursiveRow, got {err:?}"
        );
    }

    /// A probe against a duplicated-label spine binds to the *head* occurrence,
    /// since row unification walks head-first.  `infer_map_val` dedups last-wins
    /// upstream, so the shape only reaches the unifier here.
    #[test]
    fn duplicate_label_spine_matches_head() {
        let mut u = Unifier::new();
        let alpha = u.fresh_ty();
        let rho = u.fresh_row_var();
        let dup = record_row(&[("x", Ty::Int), ("x", Ty::String)]);
        let probe = open_row(&[("x", alpha.clone())], rho);
        u.unify_row(&probe, &dup)
            .expect("probe unifies against a duplicated-label spine");
        assert_eq!(
            u.apply_ty(&alpha),
            Ty::Int,
            "probe binds to the head (first) occurrence"
        );
    }

    fn kind_refusal(result: Result<(), TypeErrorKind>) -> (KindFound, Kind) {
        match result {
            Err(TypeErrorKind::KindMismatch { found, kind, .. }) => (found, kind),
            other => panic!("expected a kind refusal, got {other:?}"),
        }
    }

    fn kind_of(u: &Unifier, ty: &Ty) -> Kind {
        let Ty::Var(v) = u.resolve_ty(ty) else {
            panic!("expected a free variable, got {ty:?}")
        };
        u.kinded(v).kind
    }

    #[test]
    fn two_variables_unite_at_the_meet_of_their_kinds() {
        let mut u = Unifier::new();
        let a = u.fresh_kinded(Kind::COMPARABLE);
        let b = u.fresh_kinded(Kind::NUMBER);
        u.unify_ty(&a, &b).expect("a number is comparable");
        assert_eq!(kind_of(&u, &a), Kind::NUMBER);
        assert_eq!(kind_of(&u, &b), Kind::NUMBER);
        u.unify_ty(&a, &Ty::Int).expect("Int is a number");
        assert_eq!(u.resolve_ty(&b), Ty::Int);
    }

    #[test]
    fn a_meet_with_one_nullary_head_binds_the_variable() {
        let mut u = Unifier::new();
        let a = u.fresh_kinded(Kind::SIZED);
        let b = u.fresh_kinded(Kind::COMPARABLE);
        u.unify_ty(&a, &b).expect("text is both");
        assert_eq!(u.resolve_ty(&a), Ty::String);
        assert_eq!(u.resolve_ty(&b), Ty::String);
    }

    #[test]
    fn an_empty_meet_is_refused_citing_both_witnesses() {
        let mut u = Unifier::new();
        let (first, second) = (
            Span::point(FileId::DUMMY, 0),
            Span::new(FileId::DUMMY, 3, 9),
        );
        u.at = Some(first);
        let a = u.fresh_kinded(Kind::NUMBER);
        u.at = Some(second);
        let b = u.fresh_kinded(Kind::SIZED);
        match u.unify_ty(&a, &b) {
            Err(TypeErrorKind::KindMismatch {
                found: KindFound::Used { kind, witness },
                kind: required,
                witness: later,
            }) => {
                assert_eq!((kind, witness), (Kind::NUMBER, Some(first)));
                assert_eq!((required, later), (Kind::SIZED, Some(second)));
            }
            other => panic!("expected an empty meet, got {other:?}"),
        }
    }

    #[test]
    fn binding_a_kinded_variable_checks_the_head_and_keeps_the_kind_on_refusal() {
        let mut u = Unifier::new();
        let a = u.fresh_kinded(Kind::NUMBER);
        let (found, kind) = kind_refusal(u.unify_ty(&a, &Ty::String));
        assert!(matches!(found, KindFound::Type(ty) if *ty == Ty::String));
        assert_eq!(kind, Kind::NUMBER);
        assert_eq!(kind_of(&u, &a), Kind::NUMBER);
        u.unify_ty(&a, &Ty::Float).expect("Float is a number");
    }

    #[test]
    fn a_deep_kind_reaches_a_list_element() {
        let mut u = Unifier::new();
        let data = u.fresh_kinded(Kind::DATA);
        let elem = u.fresh_ty();
        u.unify_ty(&data, &Ty::list(elem.clone()))
            .expect("a list of something");
        assert_eq!(kind_of(&u, &elem), Kind::DATA);
        let block = Ty::Thunk(Box::new(CompTy::pure(Ty::Int)));
        let (found, kind) = kind_refusal(u.unify_ty(&elem, &block));
        assert!(matches!(found, KindFound::Type(ty) if matches!(*ty, Ty::Thunk(_))));
        assert_eq!(kind, Kind::DATA);
    }

    #[test]
    fn a_deep_kind_refuses_a_block_in_what_it_is_bound_to() {
        let mut u = Unifier::new();
        let data = u.fresh_kinded(Kind::DATA);
        let block = Ty::Thunk(Box::new(CompTy::pure(Ty::Int)));
        kind_refusal(u.unify_ty(&data, &Ty::list(block)));
    }

    #[test]
    fn a_deep_kind_reaches_a_records_fields_and_its_open_tail() {
        let mut u = Unifier::new();
        let data = u.fresh_kinded(Kind::DATA);
        let (field, tail) = (u.fresh_ty(), u.fresh_row_var());
        u.unify_ty(&data, &Ty::Record(open_row(&[("a", field.clone())], tail)))
            .expect("a record with a field of something");
        assert_eq!(kind_of(&u, &field), Kind::DATA);
        assert!(u.is_deep_row(tail));

        let block = Ty::Thunk(Box::new(CompTy::pure(Ty::Int)));
        let later = record_row(&[("b", block)]);
        kind_refusal(u.unify_row(&Row::Var(tail), &later));

        let fine = record_row(&[("b", Ty::Int)]);
        u.unify_row(&Row::Var(tail), &fine)
            .expect("an Int field is data");
    }

    #[test]
    fn a_deep_kind_reaches_a_variants_payloads() {
        let mut u = Unifier::new();
        let data = u.fresh_kinded(Kind::DATA);
        let (payload, tail) = (u.fresh_ty(), u.fresh_row_var());
        let tag = Row::Extend(
            Label::Case("ok".into()),
            Box::new(payload.clone()),
            Box::new(Row::Var(tail)),
        );
        u.unify_ty(&data, &Ty::Variant(tag))
            .expect("a variant with a payload of something");
        assert_eq!(kind_of(&u, &payload), Kind::DATA);
        assert!(u.is_deep_row(tail));
    }

    #[test]
    fn a_deep_kind_over_a_data_cycle_terminates() {
        let mut u = Unifier::new();
        let data = u.fresh_kinded(Kind::DATA);
        u.unify_ty(&data, &Ty::map(data.clone()))
            .expect("a map of itself is data all the way down");
        let other = u.fresh_kinded(Kind::DATA);
        u.unify_ty(&other, &data).expect("the same tree, twice");
    }

    #[test]
    fn two_deep_rows_unite_deep_and_a_shallow_one_becomes_so() {
        let mut u = Unifier::new();
        let (deep, shallow) = (u.fresh_deep_row_var(true), u.fresh_row_var());
        u.unify_row(&Row::Var(shallow), &Row::Var(deep))
            .expect("two row variables unite");
        assert!(u.is_deep_row(shallow) && u.is_deep_row(deep));
    }
}
