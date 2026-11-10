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
//! Row spines stay finite; row-store bindings use type-variable payloads so
//! cycles through fields always meet one of those anchors.

use super::error::{CycleVia, KindFound, TypeErrorKind};
use crate::source::Span;
use crate::ty::{
    CompTy, CompTyNode, CompTyVar, Grade, GradeVar, Head, Kind, Label, Row, RowNode, RowVar, Term,
    Ty, TyNode, TyVar, Var,
};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::ops::ControlFlow;
use std::sync::Arc;

/// What made a variable weak, so a mismatch on it can say which use shares it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WeakSource {
    /// The result of a boundary, named as the program spells it.
    Boundary(Arc<str>),
    /// A host binding without a scheme.
    Host(Arc<str>),
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

/// The free variables a `data` obligation lands on.
#[derive(Default)]
struct Owed {
    tys: Vec<TyVar>,
    rows: Vec<RowVar>,
}

/// Equality obligations already in progress; re-entering one is an immediate
/// success, which is what makes two cyclic types terminate.  Alongside the
/// symmetric root pairs are *one-sided* obligations — a bound root against the
/// other side at roots (see [`Unifier::shallow_ty`]) — because a value-anchored
/// stream (`T = Step(F T)`) meets its comp-anchored twin (`C = F Step(C)`) as
/// `Var`-vs-structure, never as a `Var`/`Var` pair.
#[derive(Default)]
struct Pairs {
    vars: HashSet<(Var, Var)>,
    tys: HashSet<(TyVar, Ty)>,
    comps: HashSet<(CompTyVar, CompTy)>,
}

/// Ceiling on *true nesting depth*: descents into a strictly deeper subterm.
/// Walking a row spine sideways is width and costs nothing, so a wide-but-
/// shallow record unifies freely.  [`Pairs`] terminates every *cyclic*
/// obligation; this is the structural stop for a variable-free type, turning a
/// stack overflow into a graceful [`TypeErrorKind::TypeTooDeep`].  The unify
/// arms, the one-sided obligations and binding validation spend the one
/// budget, so no descent escapes by crossing between them; a descent that
/// does not happen — identical types, ground types read shallowly — spends
/// nothing.  The value sits far
/// above real nesting yet under the ~900 frames that exhaust a 2 MiB stack.
const MAX_UNIFY_DEPTH: u32 = 512;

/// Charge one level against [`MAX_UNIFY_DEPTH`].  Called only on a step into a
/// strictly deeper subterm, which is why `depth` is threaded by hand below.
fn deeper(depth: u32) -> Result<u32, TypeErrorKind> {
    if depth >= MAX_UNIFY_DEPTH {
        return Err(TypeErrorKind::TypeTooDeep);
    }
    Ok(depth + 1)
}

/// A sort with variables in a union-find store.  A store's ids index only its
/// own slots; `Meta` is what a free root knows beyond its rank.
trait Sort: Clone {
    type Var: Copy + Eq + std::hash::Hash;
    type Meta: Copy;
    /// Why two free roots cannot unite.
    type Clash;
    /// The meta of a root nothing constrains.
    const FREE: Self::Meta;
    fn raw(var: Self::Var) -> u32;
    fn var(raw: u32) -> Self::Var;
    /// The variable a bare one is.
    fn as_var(&self) -> Option<Self::Var>;
    fn from_root(root: Self::Var) -> Self;
    /// What the survivor of uniting `absorbed` into `survivor` knows.
    fn meet(absorbed: Self::Meta, survivor: Self::Meta) -> Result<Self::Meta, Self::Clash>;
    /// The one value a meta admits, if it admits only one.
    fn pin(_meta: &Self::Meta) -> Option<Self> {
        None
    }
}

macro_rules! sort_vars {
    (@ids $var:ident) => {
        type Var = $var;
        fn raw(var: $var) -> u32 {
            var.0
        }
        fn var(raw: u32) -> $var {
            $var(raw)
        }
    };
    ($var:ident) => {
        sort_vars!(@ids $var);
        fn as_var(&self) -> Option<$var> {
            match self {
                Self::Var(v) => Some(*v),
                _ => None,
            }
        }
        fn from_root(root: $var) -> Self {
            Self::Var(root)
        }
    };
    // A handle: its variable is a layer of its node, and `Self::var` is the
    // handle's own constructor, not [`Sort::var`].
    ($var:ident, $node:ident) => {
        sort_vars!(@ids $var);
        fn as_var(&self) -> Option<$var> {
            match self.node() {
                $node::Var(v) => Some(*v),
                _ => None,
            }
        }
        fn from_root(root: $var) -> Self {
            Self::var(root)
        }
    };
}

impl Sort for Ty {
    sort_vars!(TyVar, TyNode);
    type Meta = Kinded;
    type Clash = TypeErrorKind;
    const FREE: Kinded = Kinded {
        kind: Kind::ANY,
        witness: None,
    };
    fn meet(a: Kinded, b: Kinded) -> Result<Kinded, TypeErrorKind> {
        let Some(kind) = a.kind.meet(b.kind) else {
            return Err(TypeErrorKind::KindMismatch {
                found: KindFound::Used {
                    kind: a.kind,
                    witness: a.witness,
                },
                kind: b.kind,
                witness: b.witness,
            });
        };
        let witness = if kind == a.kind { a.witness } else { b.witness };
        Ok(Kinded { kind, witness })
    }
    fn pin(meta: &Kinded) -> Option<Self> {
        meta.kind.pin()
    }
}

/// Whether a free row's fields must be data, with the use that made them so.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Depth {
    Shallow,
    Deep(Option<Span>),
}

impl Sort for Row {
    sort_vars!(RowVar, RowNode);
    type Meta = Depth;
    type Clash = Infallible;
    const FREE: Depth = Depth::Shallow;
    fn meet(absorbed: Depth, survivor: Depth) -> Result<Depth, Infallible> {
        Ok(match survivor {
            Depth::Shallow => absorbed,
            deep @ Depth::Deep(_) => deep,
        })
    }
}

macro_rules! bare_sort {
    ($($ty:ident => [$($vars:tt)*]),*) => {$(
        impl Sort for $ty {
            sort_vars!($($vars)*);
            type Meta = ();
            type Clash = Infallible;
            const FREE: () = ();
            fn meet((): (), (): ()) -> Result<(), Infallible> {
                Ok(())
            }
        }
    )*};
}
bare_sort!(CompTy => [CompTyVar, CompTyNode], Grade => [GradeVar]);

/// What a root stands for.
#[derive(Clone)]
enum State<T: Sort> {
    Free(T::Meta),
    /// With the use in force when it was bound, if any.
    Bound(T, Option<Span>),
}

#[derive(Clone)]
enum Slot<T: Sort> {
    Parent(T::Var),
    Root {
        rank: u32,
        /// Diagnostic provenance, independent of the rank-zero weak invariant.
        weak: Option<WeakSource>,
        state: State<T>,
    },
}

#[derive(Clone)]
struct Store<T: Sort> {
    slots: Vec<Slot<T>>,
    /// Roots bound, united, refined or lowered since the last drain; may repeat.
    touched: Vec<T::Var>,
}

impl<T: Sort> Store<T> {
    fn new() -> Self {
        Self {
            slots: Vec::new(),
            touched: Vec::new(),
        }
    }

    /// A free state, or the binding a meta that admits one value forces.
    fn free(meta: T::Meta) -> State<T> {
        T::pin(&meta).map_or(State::Free(meta), |value| State::Bound(value, None))
    }

    fn fresh(&mut self, rank: u32, meta: T::Meta) -> T::Var {
        let id = u32::try_from(self.slots.len()).expect("variable store exhausted");
        self.slots.push(Slot::Root {
            rank,
            weak: None,
            state: Self::free(meta),
        });
        T::var(id)
    }

    fn index(&self, v: T::Var) -> usize {
        let index = T::raw(v) as usize;
        assert!(
            index < self.slots.len(),
            "unallocated variable {index} in live store"
        );
        index
    }

    fn find(&mut self, v: T::Var) -> T::Var {
        let index = self.index(v);
        match self.slots[index] {
            Slot::Parent(p) => {
                let r = self.find(p);
                self.slots[index] = Slot::Parent(r);
                r
            }
            Slot::Root { .. } => v,
        }
    }

    /// [`Self::find`] without path compression, for the read-only traversals.
    fn root(&self, mut v: T::Var) -> T::Var {
        while let Slot::Parent(p) = self.slots[self.index(v)] {
            v = p;
        }
        v
    }

    fn at_root(&self, v: T::Var) -> (u32, Option<&WeakSource>, &State<T>) {
        match &self.slots[T::raw(self.root(v)) as usize] {
            Slot::Root { rank, weak, state } => (*rank, weak.as_ref(), state),
            Slot::Parent(_) => unreachable!("a root is not a parent"),
        }
    }

    fn at_root_mut(&mut self, v: T::Var) -> (&mut u32, &mut Option<WeakSource>, &mut State<T>) {
        let index = T::raw(self.root(v)) as usize;
        match &mut self.slots[index] {
            Slot::Root { rank, weak, state } => (rank, weak, state),
            Slot::Parent(_) => unreachable!("a root is not a parent"),
        }
    }

    fn get_ref(&self, v: T::Var) -> Option<&T> {
        match self.at_root(v).2 {
            State::Bound(t, _) => Some(t),
            State::Free(_) => None,
        }
    }

    fn meta(&self, v: T::Var) -> Option<&T::Meta> {
        match self.at_root(v).2 {
            State::Free(meta) => Some(meta),
            State::Bound(..) => None,
        }
    }

    fn rank(&self, v: T::Var) -> u32 {
        self.at_root(v).0
    }

    /// Whether the rank fell; a fallen rank or new provenance touches the root.
    fn lower(&mut self, v: T::Var, rank: u32, source: Option<&WeakSource>) -> bool {
        let root = self.root(v);
        let (current, weak, _) = self.at_root_mut(root);
        let sourced = weak.is_none() && source.is_some();
        if sourced {
            *weak = source.cloned();
        }
        let lowered = *current > rank;
        *current = (*current).min(rank);
        if lowered || sourced {
            self.touched.push(root);
        }
        lowered
    }

    /// Bind `v`'s root, consuming whatever it knew while free.
    fn bind(&mut self, v: T::Var, val: T, at: Option<Span>) {
        let root = self.root(v);
        *self.at_root_mut(root).2 = State::Bound(val, at);
        self.touched.push(root);
    }

    /// Replace a free root's meta.
    fn refine(&mut self, v: T::Var, meta: T::Meta) {
        let root = self.root(v);
        *self.at_root_mut(root).2 = Self::free(meta);
        self.touched.push(root);
    }

    /// Unite two free classes at the minimum rank.  On a clash nothing changes.
    fn unite(&mut self, a: T::Var, b: T::Var) -> Result<(), T::Clash> {
        let (a, b) = (self.find(a), self.find(b));
        if a == b {
            return Ok(());
        }
        let (Some(&absorbed), Some(&survivor)) = (self.meta(a), self.meta(b)) else {
            unreachable!("only free roots unite")
        };
        let meta = T::meet(absorbed, survivor)?;
        let index = self.index(a);
        let Slot::Root { rank, weak, .. } =
            std::mem::replace(&mut self.slots[index], Slot::Parent(b))
        else {
            unreachable!("a root is not a parent")
        };
        let (b_rank, b_weak, b_state) = self.at_root_mut(b);
        *b_rank = (*b_rank).min(rank);
        if b_weak.is_none() {
            *b_weak = weak;
        }
        *b_state = Self::free(meta);
        self.touched.extend([a, b]);
        Ok(())
    }

    fn weak_source(&self, v: T::Var) -> Option<&WeakSource> {
        self.at_root(v).1
    }

    fn is_weak(&self, v: T::Var) -> bool {
        self.rank(v) == 0
    }

    /// Follow a variable chain to canonical form.  The walk stops at the first
    /// non-variable head, so variables nested inside it — a cyclic binding's
    /// back-edges — survive untouched.  A free variable is rebuilt at its
    /// root, unless it is the root already.
    fn resolve(&self, x: &T) -> T {
        let Some(v) = x.as_var() else {
            return x.clone();
        };
        if let Some(b) = self.get_ref(v) {
            return self.resolve(b);
        }
        let root = self.root(v);
        if root == v {
            x.clone()
        } else {
            T::from_root(root)
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
    rank: u32,
    tys: Store<Ty>,
    ctys: Store<CompTy>,
    rows: Store<Row>,
    /// Grades are atomic and never weak: no kind, no cycle, no `weak` entry.
    grades: Store<Grade>,
    /// Unresolved producer grades whose payload may still be Unit, under
    /// both roots of each pair.
    producers: HashMap<Var, HashSet<(GradeVar, TyVar)>>,
    /// The span in force: the witness of any kind a unification narrows.
    pub(crate) at: Option<Span>,
}

impl Unifier {
    pub fn new() -> Self {
        Self {
            rank: 0,
            tys: Store::new(),
            ctys: Store::new(),
            rows: Store::new(),
            grades: Store::new(),
            producers: HashMap::new(),
            at: None,
        }
    }

    pub(crate) fn enter(&mut self) {
        self.rank += 1;
    }

    pub(crate) fn exit(&mut self) {
        // Young pairs are copied by instantiation, never improved after quantification.
        let Self {
            rank,
            tys,
            grades,
            producers,
            ..
        } = self;
        producers.retain(|_, pairs| {
            pairs.retain(|&(grade, payload)| {
                grades.rank(grade) < *rank && tys.rank(payload) < *rank
            });
            !pairs.is_empty()
        });
        self.rank = self.rank.checked_sub(1).expect("no region to exit");
    }

    pub(super) fn is_young(&self, var: Var) -> bool {
        self.rank_of(var) > self.rank
    }

    pub(super) fn rank(&self) -> u32 {
        self.rank
    }

    pub(super) fn rank_of(&self, var: Var) -> u32 {
        match var {
            Var::Ty(v) => self.tys.rank(v),
            Var::Comp(v) => self.ctys.rank(v),
            Var::Row(v) => self.rows.rank(v),
            Var::Grade(v) => self.grades.rank(v),
        }
    }

    pub(super) fn lower_to_scrutinee(&mut self, term: Term<'_>, scrutinee: Var) {
        let rank = self.rank_of(scrutinee);
        let source = self.weak_source_of(scrutinee).cloned();
        self.lower(term, rank, source.as_ref());
    }

    /// The already-low root is also the cycle guard.
    fn lower(&mut self, term: Term<'_>, rank: u32, source: Option<&WeakSource>) -> bool {
        if term.is_ground() {
            return false;
        }
        let mut changed = false;
        if let Some(Grade::Var(v)) = term.grade() {
            changed |= self.grades.lower(v, rank, None);
        }
        if let Some(var) = self.var_of(term) {
            let source = source
                .cloned()
                .or_else(|| self.weak_source_of(var).cloned());
            let lowered = match var {
                Var::Ty(v) => self.tys.lower(v, rank, source.as_ref()),
                Var::Comp(v) => self.ctys.lower(v, rank, source.as_ref()),
                Var::Row(v) => self.rows.lower(v, rank, source.as_ref()),
                Var::Grade(_) => unreachable!("grades are carried by terms"),
            };
            if !lowered {
                return changed;
            }
            match var {
                Var::Ty(v) => {
                    if let Some(ty) = self.tys.get_ref(v).cloned() {
                        self.lower(Term::Ty(&ty), rank, source.as_ref());
                    }
                }
                Var::Comp(v) => {
                    if let Some(ty) = self.ctys.get_ref(v).cloned() {
                        self.lower(Term::Comp(&ty), rank, source.as_ref());
                    }
                }
                Var::Row(v) => {
                    if let Some(row) = self.rows.get_ref(v).cloned() {
                        self.lower(Term::Row(&row), rank, source.as_ref());
                    }
                }
                Var::Grade(_) => unreachable!("grades are carried by terms"),
            }
            return true;
        }
        for child in term.children() {
            changed |= self.lower(child, rank, source);
        }
        changed
    }

    /// Keep bound roots in the equation: resolving first would lose their rank.
    fn lower_equated(&mut self, a: Term<'_>, b: Term<'_>) {
        let vars = [self.var_of(a), self.var_of(b)];
        if let Some(rank) = vars.iter().flatten().map(|&v| self.rank_of(v)).min() {
            let source = vars
                .iter()
                .flatten()
                .find_map(|&v| self.weak_source_of(v))
                .cloned();
            self.lower(a, rank, source.as_ref());
            self.lower(b, rank, source.as_ref());
        }
    }

    pub fn fresh_tyvar(&mut self) -> TyVar {
        self.tys.fresh(self.rank, Ty::FREE)
    }
    pub(crate) fn fresh_ty(&mut self) -> Ty {
        Ty::var(self.fresh_tyvar())
    }

    /// An opaque alias adds no equation or cycle: the root did not exist in `ty`.
    pub(super) fn fresh_alias(&mut self, ty: &Ty) -> Ty {
        if matches!(ty.node(), TyNode::Var(_)) {
            return ty.clone();
        }
        let root = self.fresh_tyvar();
        self.bind_ty_root(root, ty.clone());
        Ty::var(root)
    }

    pub(super) fn excludes_unit(&self, ty: &Ty) -> bool {
        match self.resolve_ty(ty).node() {
            TyNode::Unit => false,
            TyNode::Var(v) => !self.kinded(*v).kind.admits(Head::Unit),
            _ => true,
        }
    }

    pub(super) fn remember_producers(&mut self, term: Term<'_>) {
        if term.is_ground() {
            return;
        }
        if let Some(Grade::Var(grade)) = term.grade() {
            let payload = match term {
                Term::Ty(ty) => match ty.node() {
                    TyNode::Handle(_, payload) => Some(payload),
                    _ => None,
                },
                Term::Comp(cty) => match cty.node() {
                    CompTyNode::Return(_, payload) => Some(payload),
                    _ => None,
                },
                Term::Row(_) => None,
            };
            let Some(payload) = payload else {
                unreachable!("only producers carry grades")
            };
            self.improve(grade, payload);
        }
        for child in term.children() {
            self.remember_producers(child);
        }
    }

    /// `w` only pairs with Unit, so a payload that excludes it fixes the grade
    /// now; otherwise the pair waits on both roots.  This is improvement,
    /// never a default.
    fn improve(&mut self, grade: GradeVar, payload: &Ty) {
        let Grade::Var(grade) = self.resolve_grade(Grade::Var(grade)) else {
            return;
        };
        if self.excludes_unit(payload) {
            self.bind_grade(grade, Grade::Value);
        } else if let TyNode::Var(root) = *self.resolve_ty(payload).node() {
            // The grade retains its payload, not the other way round.
            self.lower(Term::Ty(&Ty::var(root)), self.grades.rank(grade), None);
            for key in [Var::Grade(grade), Var::Ty(root)] {
                self.producers.entry(key).or_default().insert((grade, root));
            }
        }
    }

    /// Revisit the pairs either of whose roots is in `touched`.
    pub(super) fn improve_producers(&mut self, touched: &[Var]) {
        let woken: HashSet<_> = touched
            .iter()
            .filter_map(|root| self.producers.remove(root))
            .flatten()
            .collect();
        for &(grade, payload) in &woken {
            for key in [Var::Grade(grade), Var::Ty(payload)] {
                if let Entry::Occupied(mut twins) = self.producers.entry(key) {
                    twins.get_mut().remove(&(grade, payload));
                    if twins.get().is_empty() {
                        twins.remove();
                    }
                }
            }
        }
        for (grade, payload) in woken {
            self.improve(grade, &Ty::var(payload));
        }
    }

    /// Roots bound, united, refined or lowered since the last call; may repeat.
    pub(super) fn take_touched(&mut self) -> Vec<Var> {
        let Self {
            tys,
            ctys,
            rows,
            grades,
            ..
        } = self;
        let tys = tys.touched.drain(..).map(Var::Ty);
        let comps = ctys.touched.drain(..).map(Var::Comp);
        let rows = rows.touched.drain(..).map(Var::Row);
        let grades = grades.touched.drain(..).map(Var::Grade);
        tys.chain(comps).chain(rows).chain(grades).collect()
    }

    pub(crate) fn fresh_comp_ty(&mut self) -> CompTy {
        CompTy::var(self.fresh_comp_root())
    }
    pub fn fresh_row_var(&mut self) -> RowVar {
        self.rows.fresh(self.rank, Row::FREE)
    }
    pub(crate) fn fresh_row(&mut self) -> Row {
        Row::var(self.fresh_row_var())
    }

    pub fn fresh_grade_var(&mut self) -> GradeVar {
        self.grades.fresh(self.rank, Grade::FREE)
    }
    pub(crate) fn fresh_grade(&mut self) -> Grade {
        Grade::Var(self.fresh_grade_var())
    }

    pub(crate) fn resolve_grade(&self, grade: Grade) -> Grade {
        self.grades.resolve(&grade)
    }

    /// Bind a free grade variable.  Its root is taken here, so the caller
    /// may hand in any variable of the class.
    pub(crate) fn bind_grade(&mut self, v: GradeVar, grade: Grade) {
        let root = self.grades.find(v);
        match self.resolve_grade(grade) {
            Grade::Var(other) => {
                let Ok(()) = self.grades.unite(root, other);
            }
            grade => self.grades.bind(root, grade, None),
        }
    }

    /// A fresh variable of `kind`, narrowed at the span in force.
    pub(crate) fn fresh_kinded_var(&mut self, kind: Kind) -> TyVar {
        let witness = if kind.is_any() { None } else { self.at };
        self.tys.fresh(self.rank, Kinded { kind, witness })
    }

    pub(crate) fn fresh_kinded(&mut self, kind: Kind) -> Ty {
        Ty::var(self.fresh_kinded_var(kind))
    }

    pub(crate) fn fresh_deep_row_var(&mut self, deep: bool) -> RowVar {
        let depth = if deep {
            Depth::Deep(self.at)
        } else {
            Depth::Shallow
        };
        self.rows.fresh(self.rank, depth)
    }

    /// The kind a free variable carries, and the span that narrowed it.
    pub(crate) fn kinded(&self, v: TyVar) -> Kinded {
        self.tys.meta(v).copied().unwrap_or(Ty::FREE)
    }

    pub(crate) fn is_deep_row(&self, v: RowVar) -> bool {
        matches!(self.rows.meta(v), Some(Depth::Deep(_)))
    }

    /// The use that fixed the structure a type-variable root stands for.
    pub(crate) fn bound_witness(&self, root: TyVar) -> Option<Span> {
        match self.tys.at_root(root).2 {
            State::Bound(_, at) => *at,
            State::Free(_) => None,
        }
    }

    /// Narrow a free variable to the meet of its kind and `kind`.
    fn narrow(
        &mut self,
        root: TyVar,
        kind: Kind,
        witness: Option<Span>,
    ) -> Result<(), TypeErrorKind> {
        let Some(&current) = self.tys.meta(root) else {
            return Ok(());
        };
        let meet = Ty::meet(current, Kinded { kind, witness })?;
        if meet.kind != current.kind {
            self.tys.refine(root, meet);
        }
        Ok(())
    }

    fn kind_error(&self, found: &Ty, kind: Kind, witness: Option<Span>) -> TypeErrorKind {
        TypeErrorKind::KindMismatch {
            found: KindFound::Type(self.apply_ty(found)),
            kind,
            witness,
        }
    }

    /// Whether `val` may become the structure `root` stands for: its head is
    /// one the kind admits, and a deep kind's components are data.
    fn admit(&mut self, root: TyVar, val: &Ty, depth: u32) -> Result<(), TypeErrorKind> {
        let Some(&Kinded { kind, witness }) = self.tys.meta(root) else {
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
        Ok(())
    }

    /// The same, for a deep row variable about to become `val`.
    fn admit_row(&mut self, root: RowVar, val: &Row, depth: u32) -> Result<(), TypeErrorKind> {
        let Some(&Depth::Deep(witness)) = self.rows.meta(root) else {
            return Ok(());
        };
        let mut owed = Owed::default();
        self.data_row(val, witness, &mut HashSet::new(), &mut owed, depth)?;
        self.impose_data(owed, witness)
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
        if let TyNode::Var(v) = ty.node()
            && !seen.insert(self.tys.root(*v))
        {
            return Ok(());
        }
        let depth = deeper(depth)?;
        let head = self.resolve_ty(ty);
        match head.node() {
            TyNode::Var(v) => owed.tys.push(*v),
            TyNode::Unit
            | TyNode::Bool
            | TyNode::Int
            | TyNode::Float
            | TyNode::String
            | TyNode::Bytes => {}
            TyNode::List(a) | TyNode::Map(a) => self.data_ty(a, witness, seen, owed, depth)?,
            TyNode::Record(r) | TyNode::Variant(r) => {
                self.data_row(r, witness, seen, owed, depth)?;
            }
            TyNode::Thunk(_) | TyNode::Handle(..) => {
                return Err(self.kind_error(&head, Kind::DATA, witness));
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
        match self.resolve_row(row).node() {
            RowNode::Empty => Ok(()),
            RowNode::Var(v) => {
                owed.rows.push(*v);
                Ok(())
            }
            RowNode::Extend(_, ty, rest) => {
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
            if self.rows.meta(root) == Some(&Depth::Shallow) {
                self.rows.refine(root, Depth::Deep(witness));
            }
        }
        Ok(())
    }

    /// One type per unit, with provenance for diagnostics.
    pub(crate) fn mark_weak(&mut self, ty: &Ty, source: &WeakSource) {
        self.lower(Term::Ty(ty), 0, Some(source));
    }

    /// A residual an earlier unit stored.  Grades carry no provenance.
    pub(crate) fn mark_residual(&mut self, var: Var) {
        let source = Some(&WeakSource::Residual);
        match var {
            Var::Ty(v) => self.lower(Term::Ty(&Ty::var(v)), 0, source),
            Var::Comp(v) => self.lower(Term::Comp(&CompTy::var(v)), 0, source),
            Var::Row(v) => self.lower(Term::Row(&Row::var(v)), 0, source),
            Var::Grade(v) => self.grades.lower(v, 0, None),
        };
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
            Term::Ty(ty) => match ty.node() {
                TyNode::Var(v) => Some(Var::Ty(self.tys.root(*v))),
                _ => None,
            },
            Term::Comp(cty) => match cty.node() {
                CompTyNode::Var(v) => Some(Var::Comp(self.ctys.root(*v))),
                _ => None,
            },
            Term::Row(row) => match row.node() {
                RowNode::Var(v) => Some(Var::Row(self.rows.root(*v))),
                _ => None,
            },
        }
    }

    /// Whether `var`, a root, stands for nothing yet.
    pub(super) fn is_unbound(&self, var: Var) -> bool {
        match var {
            Var::Ty(v) => self.tys.get_ref(v).is_none(),
            Var::Comp(v) => self.ctys.get_ref(v).is_none(),
            Var::Row(v) => self.rows.get_ref(v).is_none(),
            Var::Grade(v) => self.grades.get_ref(v).is_none(),
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
        if term.is_ground() {
            return ControlFlow::Continue(());
        }
        if let Some(grade) = term.grade()
            && let Grade::Var(v) = self.resolve_grade(grade)
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
        self.ctys.fresh(self.rank, CompTy::FREE)
    }

    /// Mirror of `fresh_comp_root` for cyclic ty bindings.
    pub(crate) fn fresh_ty_root(&mut self) -> TyVar {
        self.fresh_tyvar()
    }

    /// Pairs with `fresh_comp_root`: the scheme's snapshot, substituted.
    pub(crate) fn bind_comp_root(&mut self, root: CompTyVar, value: CompTy) {
        self.lower(
            Term::Comp(&value),
            self.ctys.rank(root),
            self.ctys.weak_source(root).cloned().as_ref(),
        );
        self.remember_producers(Term::Comp(&value));
        self.ctys.bind(root, value, None);
    }

    /// Mirror of `bind_comp_root` for cyclic ty bindings.
    pub(crate) fn bind_ty_root(&mut self, root: TyVar, value: Ty) {
        self.bind_ty_at(root, value, None);
    }

    /// `at` is the use that imposed `value`, if any.
    fn bind_ty_at(&mut self, root: TyVar, value: Ty, at: Option<Span>) {
        self.lower(
            Term::Ty(&value),
            self.tys.rank(root),
            self.tys.weak_source(root).cloned().as_ref(),
        );
        self.remember_producers(Term::Ty(&value));
        self.tys.bind(root, value, at);
    }

    /// The root's binding with substitutions applied, or `None` if unbound or a
    /// bare `Var`; `generalize` snapshots cyclic bindings this way.  Quote *from
    /// the root*, never the stored body: one level below the anchor unrolls the
    /// cycle before the back-edge fires, so the snapshot comes out off by a
    /// level and leaks the original union-find slot there.
    pub(crate) fn resolved_comp_root_binding(&self, root: CompTyVar) -> Option<CompTy> {
        match self.ctys.get_ref(root).map(CompTy::node) {
            Some(CompTyNode::Var(_)) | None => None,
            Some(_) => Some(self.apply_comp_ty(&CompTy::var(root))),
        }
    }

    /// Mirror of `resolved_comp_root_binding`; the same anchor-quoting applies.
    pub(crate) fn resolved_ty_root_binding(&self, root: TyVar) -> Option<Ty> {
        match self.tys.get_ref(root).map(Ty::node) {
            Some(TyNode::Var(_)) | None => None,
            Some(_) => Some(self.apply_ty(&Ty::var(root))),
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

    /// Canonicalize the head; variables nested in the result stay unresolved.
    pub(crate) fn resolve_ty(&self, ty: &Ty) -> Ty {
        self.tys.resolve(ty)
    }

    /// [`Self::resolve_ty`] for a computation type.
    pub(crate) fn resolve_comp_ty(&self, cty: &CompTy) -> CompTy {
        self.ctys.resolve(cty)
    }

    /// [`Self::resolve_comp_ty`] with a `Return`'s grade resolved too.
    pub(crate) fn resolve_comp_ty_graded(&self, cty: &CompTy) -> CompTy {
        let head = self.resolve_comp_ty(cty);
        match head.node() {
            CompTyNode::Return(grade, ty) => CompTy::ret(self.resolve_grade(*grade), ty.clone()),
            _ => head,
        }
    }

    /// [`Self::resolve_ty`] for a row.
    pub(crate) fn resolve_row(&self, row: &Row) -> Row {
        self.rows.resolve(row)
    }

    /// Every `Extend` label, unsorted, and the terminal: `Some(v)` for an open
    /// row, `None` for one closed by `Empty`.  The loop needs no cycle guard:
    /// the occurs check rejects a cyclic row binding before it is installed.
    fn row_spine(&self, row: &Row) -> (Vec<Label>, Option<RowVar>) {
        let mut labels = Vec::new();
        let mut cur = self.resolve_row(row);
        loop {
            cur = match cur.node() {
                RowNode::Extend(l, _, rest) => {
                    labels.push(l.clone());
                    self.resolve_row(rest)
                }
                RowNode::Var(v) => return (labels, Some(*v)),
                RowNode::Empty => return (labels, None),
            };
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
        if ty.is_ground() {
            return ty.clone();
        }
        // In CBPV every productive recursive value type closes through a
        // `Thunk` — μ values are finite, the recursion lives in the ν
        // computations — so a ty-back-edge onto `Thunk(Var(c))` with `c` on the
        // comp stack redirects to the comp anchor `c`, the canonical capture
        // point: `c` then lands in the scheme's `comp_ty_bindings` and gets a
        // fresh id per instantiation instead of being unrolled and shared.  A
        // cycle truly anchored at a ty-var reaches a `Variant`, not a `Thunk`,
        // and takes the plain fallback.
        let root = match ty.node() {
            TyNode::Var(v) => Some(self.tys.root(*v)),
            _ => None,
        };
        if let Some(r) = root {
            if visited.cyclic_tys.contains(&r) {
                return Ty::var(r);
            }
            if visited.tys.contains(&r) {
                // Match the raw binding, not `resolve_comp_ty_graded` of it: the
                // anchor is `C`'s root, not whatever `C` resolves to now.
                if let Some(TyNode::Thunk(b)) = self.tys.get_ref(r).map(Ty::node)
                    && let CompTyNode::Var(c) = *b.node()
                {
                    let c_root = self.ctys.root(c);
                    if visited.comps.contains(&c_root) {
                        visited.cyclic_comps.insert(c_root);
                        return Ty::thunk(CompTy::var(c_root));
                    }
                }
                visited.cyclic_tys.insert(r);
                return Ty::var(r);
            }
        }
        if visited.keep_weak
            && let Some(r) = root
            && self.tys.is_weak(r)
        {
            return Ty::var(r);
        }
        let resolved = self.resolve_ty(ty);
        if matches!(resolved.node(), TyNode::Var(_)) {
            return resolved;
        }
        if let Some(r) = root {
            visited.tys.insert(r);
        }
        let out = match resolved.node() {
            TyNode::List(a) => Ty::list(self.apply_ty_inner(a, visited)),
            TyNode::Map(a) => Ty::map(self.apply_ty_inner(a, visited)),
            TyNode::Handle(g, a) => {
                Ty::handle(self.resolve_grade(*g), self.apply_ty_inner(a, visited))
            }
            TyNode::Record(r) => Ty::record(self.apply_row_inner(r, visited)),
            TyNode::Variant(r) => Ty::variant(self.apply_row_inner(r, visited)),
            TyNode::Thunk(b) => Ty::thunk(self.apply_comp_ty_inner(b, visited)),
            TyNode::Unit
            | TyNode::Bytes
            | TyNode::Bool
            | TyNode::Int
            | TyNode::Float
            | TyNode::String => resolved.clone(),
            // Enumerated so a new constructor fails the build here rather than
            // falling through unsubstituted.
            TyNode::Var(_) => {
                unreachable!("unbound var early-returned; resolved is non-Var here")
            }
        };
        if let Some(r) = root
            && !visited.cyclic_tys.contains(&r)
        {
            visited.tys.remove(&r);
        }
        out
    }

    fn apply_comp_ty_inner(&self, cty: &CompTy, visited: &mut Visited) -> CompTy {
        if cty.is_ground() {
            return cty.clone();
        }
        // The anchor may sit on either side of the ty/comp boundary, but the
        // cycle traverses both, so every root currently expanding belongs to it
        // and must enter its bindings list to be given a fresh id later.
        let root = match cty.node() {
            CompTyNode::Var(v) => Some(self.ctys.root(*v)),
            _ => None,
        };
        if let Some(r) = root {
            if visited.comps.contains(&r) {
                visited.cyclic_comps.insert(r);
                return CompTy::var(r);
            }
            if visited.cyclic_comps.contains(&r) {
                return CompTy::var(r);
            }
        }
        if visited.keep_weak
            && let Some(r) = root
            && self.ctys.is_weak(r)
        {
            return CompTy::var(r);
        }
        let resolved = self.resolve_comp_ty(cty);
        if matches!(resolved.node(), CompTyNode::Var(_)) {
            return resolved;
        }
        if let Some(r) = root {
            visited.comps.insert(r);
        }
        let out = match resolved.node() {
            CompTyNode::Return(grade, a) => {
                CompTy::ret(self.resolve_grade(*grade), self.apply_ty_inner(a, visited))
            }
            CompTyNode::Fun(a, b) => CompTy::fun(
                self.apply_ty_inner(a, visited),
                self.apply_comp_ty_inner(b, visited),
            ),
            CompTyNode::Var(_) => {
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
        if row.is_ground() {
            return row.clone();
        }
        if visited.keep_weak
            && let RowNode::Var(v) = row.node()
            && self.rows.is_weak(*v)
        {
            return Row::var(self.rows.root(*v));
        }
        match self.resolve_row(row).node() {
            RowNode::Empty => Row::empty(),
            RowNode::Var(v) => Row::var(*v),
            RowNode::Extend(l, ty, rest) => {
                let ty2 = self.apply_ty_inner(ty, visited);
                let rest2 = self.apply_row_inner(rest, visited);
                Row::extend(l.clone(), ty2, rest2)
            }
        }
    }

    /// Only tail edges belong to the finite row spine; payloads may recur.
    fn row_occurs<'a>(&'a self, root: RowVar, mut row: &'a Row) -> bool {
        loop {
            if row.is_ground() {
                return false;
            }
            match row.node() {
                RowNode::Empty => return false,
                RowNode::Extend(_, _, rest) => row = rest,
                RowNode::Var(v) => {
                    let v = self.rows.root(*v);
                    if v == root {
                        return true;
                    }
                    match self.rows.get_ref(v) {
                        Some(bound) => row = bound,
                        None => return false,
                    }
                }
            }
        }
    }

    /// Finite spines, with type-rooted payloads so every new cycle has an anchor.
    fn bind_row(&mut self, v: RowVar, mut row: Row, depth: u32) -> Result<(), TypeErrorKind> {
        let root = self.rows.find(v);
        if self.row_occurs(root, &row) {
            return Err(TypeErrorKind::RecursiveRow);
        }
        Self::check_binding_depth(Term::Row(&row), depth)?;
        self.admit_row(root, &row, depth)?;

        let mut fields = Vec::new();
        while let RowNode::Extend(label, payload, rest) = row.node() {
            fields.push((label.clone(), self.fresh_alias(payload)));
            row = rest.clone();
        }
        // Do not expand the terminal tail or re-anchor an existing binding.
        for (label, payload) in fields.into_iter().rev() {
            row = Row::extend(label, payload, row);
        }
        self.lower(
            Term::Row(&row),
            self.rows.rank(root),
            self.rows.weak_source(root).cloned().as_ref(),
        );
        self.rows.bind(root, row, None);
        Ok(())
    }

    // The other side of a one-sided obligation, at roots: variables become
    // their roots and grades resolve, but bindings are not expanded, so equal
    // terms mean the same obligation against the same anchor — the fixed point
    // the guard discharges — and a cyclic binding still yields a finite term.
    // Being a structural recursion, it spends the `MAX_UNIFY_DEPTH` budget
    // threaded in from the calling unify arm.

    fn shallow_ty(&self, ty: &Ty, depth: u32) -> Result<Ty, TypeErrorKind> {
        if ty.is_ground() {
            return Ok(ty.clone());
        }
        Ok(match ty.node() {
            TyNode::Unit
            | TyNode::Bytes
            | TyNode::Bool
            | TyNode::Int
            | TyNode::Float
            | TyNode::String => ty.clone(),
            TyNode::List(a) => Ty::list(self.shallow_ty(a, deeper(depth)?)?),
            TyNode::Map(a) => Ty::map(self.shallow_ty(a, deeper(depth)?)?),
            TyNode::Record(r) => Ty::record(self.shallow_row(r, deeper(depth)?)?),
            TyNode::Variant(r) => Ty::variant(self.shallow_row(r, deeper(depth)?)?),
            TyNode::Thunk(c) => Ty::thunk(self.shallow_comp(c, deeper(depth)?)?),
            TyNode::Handle(g, a) => {
                Ty::handle(self.resolve_grade(*g), self.shallow_ty(a, deeper(depth)?)?)
            }
            TyNode::Var(v) => Ty::var(self.tys.root(*v)),
        })
    }

    fn shallow_comp(&self, cty: &CompTy, depth: u32) -> Result<CompTy, TypeErrorKind> {
        if cty.is_ground() {
            return Ok(cty.clone());
        }
        Ok(match cty.node() {
            CompTyNode::Return(grade, t) => CompTy::ret(
                self.resolve_grade(*grade),
                self.shallow_ty(t, deeper(depth)?)?,
            ),
            CompTyNode::Fun(a, b) => CompTy::fun(
                self.shallow_ty(a, deeper(depth)?)?,
                self.shallow_comp(b, deeper(depth)?)?,
            ),
            CompTyNode::Var(v) => CompTy::var(self.ctys.root(*v)),
        })
    }

    fn shallow_row(&self, row: &Row, depth: u32) -> Result<Row, TypeErrorKind> {
        if row.is_ground() {
            return Ok(row.clone());
        }
        Ok(match row.node() {
            RowNode::Empty => Row::empty(),
            RowNode::Var(v) => Row::var(self.rows.root(*v)),
            RowNode::Extend(l, ty, rest) => Row::extend(
                l.clone(),
                self.shallow_ty(ty, deeper(depth)?)?,
                self.shallow_row(rest, depth)?,
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
        self.unify_ty_inner(a, b, &mut pairs, 0)?;
        self.remember_producers(Term::Ty(a));
        self.remember_producers(Term::Ty(b));
        Ok(())
    }

    fn unify_ty_inner(
        &mut self,
        a: &Ty,
        b: &Ty,
        pairs: &mut Pairs,
        depth: u32,
    ) -> Result<(), TypeErrorKind> {
        // Interned, so `==` is structural equality: nothing is left to unify,
        // and no descent is left for the depth budget to bound.  The row and
        // computation cases below stop the same way.
        if a == b {
            return Ok(());
        }
        self.lower_equated(Term::Ty(a), Term::Ty(b));
        match (a.node(), b.node()) {
            (TyNode::Var(ai), TyNode::Var(bi)) => {
                let (ar, br) = (self.tys.find(*ai), self.tys.find(*bi));
                if guard_pair(&mut pairs.vars, Var::Ty(ar), Var::Ty(br)) {
                    return Ok(());
                }
            }
            // One-sided, which is how a value-anchored cycle meets its
            // comp-anchored twin: the two never present as `Var`/`Var`.
            (TyNode::Var(vi), _) if self.tys.get_ref(*vi).is_some() => {
                let (root, key) = (self.tys.find(*vi), self.shallow_ty(b, depth)?);
                if guard_expansion(&mut pairs.tys, root, key) {
                    return Ok(());
                }
            }
            (_, TyNode::Var(vi)) if self.tys.get_ref(*vi).is_some() => {
                let (root, key) = (self.tys.find(*vi), self.shallow_ty(a, depth)?);
                if guard_expansion(&mut pairs.tys, root, key) {
                    return Ok(());
                }
            }
            _ => {}
        }

        let a = self.resolve_ty(a);
        let b = self.resolve_ty(b);

        if let (TyNode::Var(ai), TyNode::Var(bi)) = (a.node(), b.node()) {
            return self.tys.unite(*ai, *bi);
        }
        if let TyNode::Var(vi) = a.node() {
            return self.bind_ty(*vi, b, depth);
        }
        if let TyNode::Var(vi) = b.node() {
            return self.bind_ty(*vi, a, depth);
        }
        let depth = deeper(depth)?;
        match (a.node(), b.node()) {
            (TyNode::Unit, TyNode::Unit)
            | (TyNode::Bool, TyNode::Bool)
            | (TyNode::Int, TyNode::Int)
            | (TyNode::Float, TyNode::Float)
            | (TyNode::String, TyNode::String)
            | (TyNode::Bytes, TyNode::Bytes) => Ok(()),
            (TyNode::List(a1), TyNode::List(b1)) | (TyNode::Map(a1), TyNode::Map(b1)) => {
                self.unify_ty_inner(a1, b1, pairs, depth)
            }
            (TyNode::Handle(ga, a1), TyNode::Handle(gb, b1)) => {
                // Payload failures retain their budget, cycle or kind diagnosis.
                self.unify_producers((*ga, a1), (*gb, b1), pairs, depth, |u| {
                    TypeErrorKind::TyMismatch {
                        expected: u.apply_ty(&Ty::handle(*ga, a1.clone())),
                        actual: u.apply_ty(&Ty::handle(*gb, b1.clone())),
                    }
                })
            }
            (TyNode::Record(r1), TyNode::Record(r2))
            | (TyNode::Variant(r1), TyNode::Variant(r2)) => {
                let r = self.unify_row_inner(r1, r2, pairs, depth);
                self.name_alternatives(r1, r2, r)
            }
            (TyNode::Thunk(a1), TyNode::Thunk(b1)) => {
                self.unify_comp_ty_inner(a1, b1, pairs, depth)
            }
            // Enumerated, not `_`, here and in the two matches below: a new
            // constructor then fails the build until it is routed above,
            // instead of being reported as a mismatch with itself.
            (
                TyNode::Unit
                | TyNode::Bytes
                | TyNode::Bool
                | TyNode::Int
                | TyNode::Float
                | TyNode::String
                | TyNode::List(_)
                | TyNode::Map(_)
                | TyNode::Record(_)
                | TyNode::Variant(_)
                | TyNode::Thunk(_)
                | TyNode::Handle(..)
                | TyNode::Var(_),
                _,
            ) => Err(TypeErrorKind::TyMismatch {
                expected: self.apply_ty(&a),
                actual: self.apply_ty(&b),
            }),
        }
    }

    /// Bind `v`'s root to the structure `val`, unless `val` reaches it without
    /// crossing data.  Var–var unions cannot close a cycle, so binding is the
    /// only place one can form.
    fn bind_ty(&mut self, v: TyVar, val: Ty, depth: u32) -> Result<(), TypeErrorKind> {
        let root = self.tys.find(v);
        self.refuse_data_free_cycle(Var::Ty(root), Term::Ty(&val))?;
        Self::check_binding_depth(Term::Ty(&val), depth)?;
        self.admit(root, &val, depth)?;
        self.bind_ty_at(root, val, self.at);
        Ok(())
    }

    fn bind_comp_ty(&mut self, v: CompTyVar, val: CompTy, depth: u32) -> Result<(), TypeErrorKind> {
        let root = self.ctys.find(v);
        self.refuse_data_free_cycle(Var::Comp(root), Term::Comp(&val))?;
        Self::check_binding_depth(Term::Comp(&val), depth)?;
        self.bind_comp_root(root, val);
        Ok(())
    }

    /// Check binding syntax, stopping at variables. Curried and row tails are width.
    fn check_binding_depth(term: Term<'_>, depth: u32) -> Result<(), TypeErrorKind> {
        let mut pending = vec![(term, depth)];
        while let Some((term, depth)) = pending.pop() {
            let spine = match term {
                Term::Comp(cty) => match cty.node() {
                    CompTyNode::Fun(arg, rest) => Some((Term::Ty(arg), Term::Comp(rest))),
                    _ => None,
                },
                Term::Row(row) => match row.node() {
                    RowNode::Extend(_, payload, rest) => Some((Term::Ty(payload), Term::Row(rest))),
                    _ => None,
                },
                Term::Ty(_) => None,
            };
            if let Some((payload, rest)) = spine {
                pending.push((rest, depth));
                pending.push((payload, deeper(depth)?));
            } else {
                for child in term.children() {
                    pending.push((child, deeper(depth)?));
                }
            }
        }
        Ok(())
    }

    /// A graph search, not a structural equation: term depth costs no budget.
    fn refuse_data_free_cycle(&self, anchor: Var, from: Term<'_>) -> Result<(), TypeErrorKind> {
        let mut visited = Visited::default();
        let mut pending = vec![(from, CycleVia::Returns)];
        while let Some((term, via)) = pending.pop() {
            if term.is_ground() {
                continue;
            }
            // Thunks and aliases can name the function itself; other wrappers cannot.
            let via = match (via, term) {
                (CycleVia::Applied, Term::Ty(ty)) if matches!(ty.node(), TyNode::Handle(..)) => {
                    CycleVia::Argument
                }
                (CycleVia::Applied, Term::Comp(cty))
                    if matches!(cty.node(), CompTyNode::Return(..) | CompTyNode::Fun(..)) =>
                {
                    CycleVia::Argument
                }
                _ => via,
            };
            match term {
                Term::Ty(ty) => match ty.node() {
                    TyNode::Var(v) => {
                        let root = self.tys.root(*v);
                        if anchor == Var::Ty(root) {
                            return Err(TypeErrorKind::CyclicType { via });
                        }
                        if visited.tys.insert(root)
                            && let Some(ty) = self.tys.get_ref(root)
                        {
                            pending.push((Term::Ty(ty), via));
                        }
                    }
                    TyNode::Thunk(comp) => pending.push((Term::Comp(comp), via)),
                    TyNode::Handle(_, payload) => pending.push((Term::Ty(payload), via)),
                    TyNode::Unit
                    | TyNode::Bytes
                    | TyNode::Bool
                    | TyNode::Int
                    | TyNode::Float
                    | TyNode::String
                    | TyNode::List(_)
                    | TyNode::Map(_)
                    | TyNode::Record(_)
                    | TyNode::Variant(_) => {}
                },
                Term::Comp(cty) => match cty.node() {
                    CompTyNode::Var(v) => {
                        let root = self.ctys.root(*v);
                        if anchor == Var::Comp(root) {
                            return Err(TypeErrorKind::CyclicType { via });
                        }
                        if visited.comps.insert(root)
                            && let Some(comp) = self.ctys.get_ref(root)
                        {
                            pending.push((Term::Comp(comp), via));
                        }
                    }
                    CompTyNode::Return(_, payload) => {
                        pending.push((Term::Ty(payload), via));
                    }
                    CompTyNode::Fun(arg, result) => {
                        let arg_via = if via == CycleVia::Returns {
                            CycleVia::Applied
                        } else {
                            via
                        };
                        pending.push((Term::Comp(result), via));
                        pending.push((Term::Ty(arg), arg_via));
                    }
                },
                Term::Row(_) => {}
            }
        }
        Ok(())
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
        let result = self.name_alternatives(a, b, r);
        if result.is_ok() {
            self.remember_producers(Term::Row(a));
            self.remember_producers(Term::Row(b));
        }
        result
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
        let known = if a_names.contains(&label.to_string()) {
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
        let mut a = a.clone();
        let mut b = b.clone();
        loop {
            if a == b {
                return Ok(());
            }
            self.lower_equated(Term::Row(&a), Term::Row(&b));
            a = self.resolve_row(&a);
            b = self.resolve_row(&b);
            if let (RowNode::Var(ai), RowNode::Var(bi)) = (a.node(), b.node()) {
                let Ok(()) = self.rows.unite(*ai, *bi);
                return Ok(());
            }
            if let RowNode::Var(vi) = a.node() {
                return self.bind_row(*vi, b, depth);
            }
            if let RowNode::Var(vi) = b.node() {
                return self.bind_row(*vi, a, depth);
            }

            match (a.node(), b.node()) {
                (RowNode::Empty, RowNode::Empty) => return Ok(()),
                (RowNode::Empty, RowNode::Extend(l, ..)) => {
                    return Err(TypeErrorKind::RowExtraField {
                        label: l.clone(),
                        known: Vec::new(),
                    });
                }
                (RowNode::Extend(l, ..), RowNode::Empty) => {
                    return Err(TypeErrorKind::RowMissingField {
                        label: l.to_string(),
                    });
                }
                (RowNode::Extend(l1, t1, r1), RowNode::Extend(l2, t2, r2)) => {
                    if l1 == l2 {
                        self.unify_ty_inner(t1, t2, pairs, deeper(depth)?)?;
                        // In place, not a deeper frame: a wide row is O(1) stack.
                        let (rest_a, rest_b) = (r1.clone(), r2.clone());
                        a = rest_a;
                        b = rest_b;
                        continue;
                    }
                    // A field name and a constructor are different labels
                    // however they are spelled, so a row carrying both
                    // typechecks against neither pure form.
                    if std::mem::discriminant(l1) != std::mem::discriminant(l2) {
                        return Err(TypeErrorKind::TyMismatch {
                            expected: Ty::record(Row::extend(l1.clone(), t1.clone(), Row::empty())),
                            actual: Ty::record(Row::extend(l2.clone(), t2.clone(), Row::empty())),
                        });
                    }
                    // Scoped-labels side condition (Gaster–Jones, Leijen): two
                    // rows on the *same* tail with different label multisets
                    // have no finite or rational solution — the rewrite would
                    // re-enter with the mismatch intact, minting a fresh tail
                    // each turn.  The disagreement can sit below the head, so
                    // compare whole spines; a permutation does have a solution
                    // and must still take it.
                    let (mut left, left_tail) = self.row_spine(r1);
                    let (mut right, right_tail) = self.row_spine(r2);
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
                    let new_r1 = Row::extend(l2.clone(), t2.clone(), Row::var(rho));
                    let new_r2 = Row::extend(l1.clone(), t1.clone(), Row::var(rho));
                    self.unify_row_inner(r1, &new_r1, pairs, depth)?;
                    return self.unify_row_inner(&new_r2, r2, pairs, depth);
                }
                (RowNode::Var(_), _) | (_, RowNode::Var(_)) => {
                    unreachable!("RowNode::Var pairs are handled by the early-return blocks above")
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
        self.unify_comp_ty_inner(a, b, &mut pairs, 0)?;
        self.remember_producers(Term::Comp(a));
        self.remember_producers(Term::Comp(b));
        Ok(())
    }

    /// Preserve Unit capability across a grade class, even after a payload
    /// error; a [`Self::producers_clash`] is refused as `clash`.
    fn unify_producers(
        &mut self,
        a: (Grade, &Ty),
        b: (Grade, &Ty),
        pairs: &mut Pairs,
        depth: u32,
        clash: impl Fn(&Self) -> TypeErrorKind,
    ) -> Result<(), TypeErrorKind> {
        if self.producers_clash(a, b) {
            return Err(clash(self));
        }
        let unit_variable = |ty: &Ty| {
            matches!(self.resolve_ty(ty).node(), TyNode::Var(_)) && !self.excludes_unit(ty)
        };
        let payload_first = unit_variable(a.1)
            || unit_variable(b.1)
            || self.excludes_unit(a.1) != self.excludes_unit(b.1);
        if payload_first {
            // A failed payload equation must not commit a grade binding.
            // Admission can fail below an admitted head or exhaust the depth budget.
            // A free type variable or Unit is at the head of this equation, so
            // it neither unifies nested grades nor allocates fresh variables;
            // binding it may still improve a grade nested in the other side.
            self.unify_ty_inner(a.1, b.1, pairs, depth)?;
            if self.producers_clash(a, b) {
                return Err(clash(self));
            }
        }
        // No clash: a variable takes the other grade, or two constants agree.
        if let (Grade::Var(v), grade) | (grade, Grade::Var(v)) =
            (self.resolve_grade(a.0), self.resolve_grade(b.0))
        {
            self.bind_grade(v, grade);
        }
        if !payload_first {
            self.unify_ty_inner(a.1, b.1, pairs, depth)?;
        }
        Ok(())
    }

    /// Distinct constant grades, or `Output` against a payload without Unit.
    fn producers_clash(&self, (ga, ta): (Grade, &Ty), (gb, tb): (Grade, &Ty)) -> bool {
        let (ga, gb) = (self.resolve_grade(ga), self.resolve_grade(gb));
        let output_without_unit = |grade, ty| grade == Grade::Output && self.excludes_unit(ty);
        matches!(
            (ga, gb),
            (Grade::Value, Grade::Output) | (Grade::Output, Grade::Value)
        ) || output_without_unit(ga, tb)
            || output_without_unit(gb, ta)
    }

    fn unify_comp_ty_inner(
        &mut self,
        a: &CompTy,
        b: &CompTy,
        pairs: &mut Pairs,
        depth: u32,
    ) -> Result<(), TypeErrorKind> {
        if a == b {
            return Ok(());
        }
        self.lower_equated(Term::Comp(a), Term::Comp(b));
        match (a.node(), b.node()) {
            (CompTyNode::Var(ai), CompTyNode::Var(bi)) => {
                let (ar, br) = (self.ctys.find(*ai), self.ctys.find(*bi));
                if guard_pair(&mut pairs.vars, Var::Comp(ar), Var::Comp(br)) {
                    return Ok(());
                }
            }
            // One-sided: the comp half of the anchoring mismatch, `F T ~= C`.
            (CompTyNode::Var(vi), _) if self.ctys.get_ref(*vi).is_some() => {
                let (root, key) = (self.ctys.find(*vi), self.shallow_comp(b, depth)?);
                if guard_expansion(&mut pairs.comps, root, key) {
                    return Ok(());
                }
            }
            (_, CompTyNode::Var(vi)) if self.ctys.get_ref(*vi).is_some() => {
                let (root, key) = (self.ctys.find(*vi), self.shallow_comp(a, depth)?);
                if guard_expansion(&mut pairs.comps, root, key) {
                    return Ok(());
                }
            }
            _ => {}
        }

        let a = self.resolve_comp_ty_graded(a);
        let b = self.resolve_comp_ty_graded(b);

        if let (CompTyNode::Var(ai), CompTyNode::Var(bi)) = (a.node(), b.node()) {
            let Ok(()) = self.ctys.unite(*ai, *bi);
            return Ok(());
        }
        if let CompTyNode::Var(vi) = a.node() {
            return self.bind_comp_ty(*vi, b, depth);
        }
        if let CompTyNode::Var(vi) = b.node() {
            return self.bind_comp_ty(*vi, a, depth);
        }
        let depth = deeper(depth)?;
        match (a.node(), b.node()) {
            (CompTyNode::Return(ga, ta), CompTyNode::Return(gb, tb)) => {
                let mismatch = |u: &Self| TypeErrorKind::CompTyMismatch {
                    expected: u.apply_comp_ty(&CompTy::ret(*ga, ta.clone())),
                    actual: u.apply_comp_ty(&CompTy::ret(*gb, tb.clone())),
                };
                self.unify_producers((*ga, ta), (*gb, tb), pairs, depth, mismatch)
                    .map_err(|e| match e {
                        // Budget, cycle and kind failures explain more than a shape mismatch.
                        TypeErrorKind::TypeTooDeep
                        | TypeErrorKind::CyclicType { .. }
                        | TypeErrorKind::KindMismatch { .. } => e,
                        _ => mismatch(self),
                    })
            }
            (CompTyNode::Fun(a1, b1), CompTyNode::Fun(a2, b2)) => {
                self.unify_ty_inner(a1, a2, pairs, depth)?;
                self.unify_comp_ty_inner(b1, b2, pairs, depth)
            }
            (CompTyNode::Return(..) | CompTyNode::Fun(..) | CompTyNode::Var(_), _) => {
                Err(TypeErrorKind::CompTyMismatch {
                    expected: self.apply_comp_ty(&a),
                    actual: self.apply_comp_ty(&b),
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

/// Symmetric obligation: two var roots, ordered so `(a, b)` and `(b, a)` key
/// the same entry.  `true` when in progress or coincident.
fn guard_pair(seen: &mut HashSet<(Var, Var)>, a: Var, b: Var) -> bool {
    a == b || !seen.insert((a.min(b), a.max(b)))
}

/// One-sided obligation: a var root against a shallow term.  `true` when in progress.
fn guard_expansion<V: Eq + std::hash::Hash, K: Eq + std::hash::Hash>(
    seen: &mut HashSet<(V, K)>,
    root: V,
    key: K,
) -> bool {
    !seen.insert((root, key))
}

#[cfg(test)]
mod tests {
    #[test]
    fn recursive_record_equations_are_order_independent() {
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            for reversed in 0..8 {
                for allocation in [false, true] {
                    for rank in [0, 2] {
                        let mut u = Unifier::new();
                        for _ in 0..rank {
                            u.enter();
                        }
                        let mut values = [u.fresh_ty(), u.fresh_ty(), u.fresh_ty()];
                        let mut tails = [u.fresh_row_var(), u.fresh_row_var()];
                        if allocation {
                            values.reverse();
                            tails.reverse();
                        }
                        let [r, x, y] = values;
                        let [rho, sigma] = tails;
                        let r_shape = Ty::record(open_row(&[("a", x.clone())], rho));
                        let x_shape = Ty::record(open_row(&[("b", y.clone())], sigma));
                        let equations = [(&r, &r_shape), (&x, &x_shape), (&r, &x)];
                        for index in order {
                            let (left, right) = equations[index];
                            let result = if reversed & (1 << index) == 0 {
                                u.unify_ty(left, right)
                            } else {
                                u.unify_ty(right, left)
                            };
                            assert!(
                                result.is_ok(),
                                "{order:?}/{reversed}/{allocation}/{rank}: {result:?}"
                            );
                        }
                        let TyNode::Record(row) = u.resolve_ty(&r).node().clone() else {
                            panic!("record")
                        };
                        let (_, tail) = row_parts(&u, &row);
                        assert!(matches!(tail.node(), RowNode::Var(_)));
                        for start in [&r, &x] {
                            let mut current = start.clone();
                            let mut seen = HashSet::new();
                            loop {
                                let TyNode::Var(anchor) = current.node() else {
                                    panic!("recursive payload has a type anchor")
                                };
                                if !seen.insert(u.ty_root(*anchor)) {
                                    break;
                                }
                                let TyNode::Record(row) = u.resolve_ty(&current).node().clone()
                                else {
                                    panic!("recursive record")
                                };
                                let (fields, rest) = row_parts(&u, &row);
                                assert_eq!(fields.len(), 2);
                                assert_eq!(rest, tail);
                                let a = &fields.iter().find(|(l, _)| l.name() == "a").unwrap().1;
                                let b = &fields.iter().find(|(l, _)| l.name() == "b").unwrap().1;
                                assert_eq!(u.resolve_ty(b), u.resolve_ty(&y));
                                assert!(matches!(u.resolve_ty(b).node(), TyNode::Var(_)));
                                current = a.clone();
                            }
                        }
                        // Both original tails share the one residual, not two copies.
                        u.unify_row(&tail, &record_row(&[("extra", Ty::bool())]))
                            .unwrap();
                        u.unify_ty(&y, &Ty::string()).unwrap();
                        for (row, label, payload) in
                            [(rho, "b", Ty::string()), (sigma, "a", u.resolve_ty(&x))]
                        {
                            let (fields, rest) = row_parts(&u, &Row::var(row));
                            assert_eq!(rest, Row::empty());
                            assert_eq!(
                                fields.iter().map(|(l, _)| l.name()).collect::<Vec<_>>(),
                                [label, "extra"]
                            );
                            assert_eq!(u.resolve_ty(&fields[0].1), payload);
                            assert_eq!(u.apply_ty(&fields[1].1), Ty::bool());
                        }
                    }
                }
            }
        }
    }

    fn row_parts(u: &Unifier, row: &Row) -> (Vec<(Label, Ty)>, Row) {
        let mut fields = Vec::new();
        let mut row = u.resolve_row(row);
        while let RowNode::Extend(label, payload, rest) = row.node().clone() {
            fields.push((label, payload));
            row = u.resolve_row(&rest);
        }
        (fields, row)
    }

    #[test]
    fn fresh_alias_preserves_an_existing_variables_identity_and_kind() {
        let mut u = Unifier::new();
        u.enter();
        let ty = u.fresh_kinded(Kind::NUMBER);
        let alias = u.fresh_alias(&ty);
        assert_eq!(alias, ty);
        u.unify_ty(&alias, &Ty::int()).unwrap();
        assert_eq!(u.resolve_ty(&ty), Ty::int());
    }

    #[test]
    fn producer_pairs_lower_dependencies_and_retire_before_generalisation() {
        let mut u = Unifier::new();
        u.enter();
        let old_grade = u.fresh_grade();
        u.enter();
        let young_payload = u.fresh_ty();
        let young_grade = u.fresh_grade();
        let local_payload = u.fresh_ty();
        u.remember_producers(Term::Comp(&CompTy::ret(old_grade, young_payload.clone())));
        u.remember_producers(Term::Ty(&Ty::handle(young_grade, local_payload)));
        improve(&mut u);
        let TyNode::Var(payload) = *young_payload.node() else {
            unreachable!()
        };
        assert_eq!(u.rank_of(Var::Ty(payload)), 1);
        u.exit();
        assert_eq!(producer_pairs(&u).len(), 1);
        u.unify_ty(&young_payload, &Ty::int()).unwrap();
        improve(&mut u);
        assert_eq!(u.resolve_grade(old_grade), Grade::Value);
        assert_eq!(u.resolve_grade(young_grade), young_grade);
        assert!(producer_pairs(&u).is_empty());
    }

    #[test]
    fn producer_pairs_use_kind_exclusion_but_do_not_default_unit() {
        let mut u = Unifier::new();
        u.enter();
        for kind in [Kind::ANY, Kind::NUMBER] {
            let payload = u.fresh_kinded(kind);
            let grade = u.fresh_grade();
            u.remember_producers(Term::Comp(&CompTy::ret(grade, payload)));
            // An excluding kind fixes the grade at registration.
            let expected = if kind == Kind::ANY {
                grade
            } else {
                Grade::Value
            };
            assert_eq!(u.resolve_grade(grade), expected);
            improve(&mut u);
            assert_eq!(u.resolve_grade(grade), expected);
        }
        let grade = u.fresh_grade();
        u.remember_producers(Term::Ty(&Ty::handle(grade, Ty::unit())));
        improve(&mut u);
        assert_eq!(u.resolve_grade(grade), grade);
        u.exit();
        assert!(producer_pairs(&u).is_empty());
    }

    #[test]
    fn every_store_preserves_rank_on_union_and_binding() {
        fn check<T: Sort>(value: &T) {
            for old_first in [false, true] {
                let mut store = Store::<T>::new();
                let old = store.fresh(1, T::FREE);
                let young = store.fresh(2, T::FREE);
                let united = if old_first {
                    store.unite(old, young)
                } else {
                    store.unite(young, old)
                };
                assert!(united.is_ok());
                assert_eq!(store.rank(old), 1);
                assert_eq!(store.rank(young), 1);
                store.bind(young, value.clone(), None);
                assert_eq!(store.rank(old), 1);
                assert!(store.get_ref(old).is_some());
            }
        }
        check(&Ty::int());
        check(&CompTy::pure(Ty::int()));
        check(&Row::empty());
        check(&Grade::Value);
    }

    #[test]
    fn structural_binding_lowers_bound_roots_and_cycles_in_every_sort() {
        let mut u = Unifier::new();
        u.enter();
        let old = u.fresh_ty();
        u.enter();
        let ty = u.fresh_tyvar();
        let comp = u.fresh_comp_root();
        let row = u.fresh_row_var();
        let grade = u.fresh_grade_var();
        u.bind_comp_root(comp, CompTy::ret(Grade::Var(grade), Ty::var(ty)));
        u.bind_ty_root(
            ty,
            Ty::list(Ty::record(Row::extend(
                Label::Field("next".into()),
                Ty::thunk(CompTy::var(comp)),
                Row::var(row),
            ))),
        );
        u.unify_ty(&old, &Ty::var(ty)).unwrap();
        for var in [
            Var::Ty(ty),
            Var::Comp(comp),
            Var::Row(row),
            Var::Grade(grade),
        ] {
            assert_eq!(u.rank_of(var), 1);
        }
        let source = WeakSource::Boundary("from-json".into());
        u.mark_weak(&old, &source);
        for var in [
            Var::Ty(ty),
            Var::Comp(comp),
            Var::Row(row),
            Var::Grade(grade),
        ] {
            assert_eq!(u.rank_of(var), 0);
        }
        assert_eq!(u.weak_source_in_ty(&Ty::var(ty)), Some(source));
    }

    #[test]
    fn binding_uniting_and_narrowing_touch_roots() {
        let mut u = Unifier::new();
        let (a, b) = (u.fresh_tyvar(), u.fresh_tyvar());
        let data = u.fresh_kinded_var(Kind::DATA);
        let elem = u.fresh_tyvar();
        let grade = u.fresh_grade_var();
        assert_eq!(u.take_touched(), []);
        u.unify_ty(&Ty::var(a), &Ty::var(b)).unwrap();
        u.unify_ty(&Ty::var(data), &Ty::list(Ty::var(elem)))
            .unwrap();
        u.remember_producers(Term::Comp(&CompTy::ret(Grade::Var(grade), Ty::int())));
        assert!(!u.is_unbound(Var::Grade(grade)));
        let touched = u.take_touched();
        for var in [a, b, data, elem]
            .map(Var::Ty)
            .into_iter()
            .chain([Var::Grade(grade)])
        {
            assert!(touched.contains(&var), "{var:?} untouched in {touched:?}");
        }
        assert_eq!(u.take_touched(), []);
    }

    #[test]
    fn residuals_are_weak_with_residual_provenance() {
        let mut u = Unifier::new();
        u.enter();
        let (ty, row) = (u.fresh_tyvar(), u.fresh_row_var());
        u.mark_residual(Var::Ty(ty));
        u.mark_residual(Var::Row(row));
        assert!(u.is_weak_ty(ty) && u.is_weak_row(row));
        assert_eq!(u.weak_source_of(Var::Ty(ty)), Some(&WeakSource::Residual));
        assert_eq!(u.weak_source_of(Var::Row(row)), Some(&WeakSource::Residual));
    }

    #[test]
    fn equations_keep_the_rank_of_already_bound_roots() {
        let mut u = Unifier::new();
        u.enter();
        let old = u.fresh_ty();
        u.unify_ty(&old, &Ty::int()).unwrap();
        u.enter();
        let young = u.fresh_tyvar();
        u.unify_ty(&Ty::var(young), &old).unwrap();
        assert_eq!(u.rank_of(Var::Ty(young)), 1);
    }

    #[test]
    #[should_panic(expected = "unallocated variable")]
    fn find_rejects_foreign_ids() {
        Store::<Ty>::new().find(TyVar(0));
    }

    #[test]
    #[should_panic(expected = "unallocated variable")]
    fn root_rejects_foreign_ids() {
        Store::<Ty>::new().root(TyVar(0));
    }

    use super::*;
    use crate::source::FileId;

    /// Improve producers to a fixed point, as solving does.
    fn improve(u: &mut Unifier) {
        loop {
            let touched = u.take_touched();
            if touched.is_empty() {
                return;
            }
            u.improve_producers(&touched);
        }
    }

    fn producer_pairs(u: &Unifier) -> HashSet<(GradeVar, TyVar)> {
        u.producers.values().flatten().copied().collect()
    }

    fn unify_test_producers(
        u: &mut Unifier,
        handles: bool,
        a: (Grade, &Ty),
        b: (Grade, &Ty),
    ) -> Result<(), TypeErrorKind> {
        if handles {
            u.unify_ty(&Ty::handle(a.0, a.1.clone()), &Ty::handle(b.0, b.1.clone()))
        } else {
            u.unify_comp_ty(
                &CompTy::ret(a.0, a.1.clone()),
                &CompTy::ret(b.0, b.1.clone()),
            )
        }
    }

    #[test]
    fn ty_mismatch_applies_nested_substitutions_without_defaulting_unknowns() {
        for known in [false, true] {
            for reversed in [false, true] {
                let mut u = Unifier::new();
                let payload = u.fresh_ty();
                if known {
                    u.unify_ty(&payload, &Ty::int()).unwrap();
                }
                let list = Ty::list(payload.clone());
                let resolved = Ty::list(if known { Ty::int() } else { payload });
                let (a, b, expected, actual) = if reversed {
                    (&Ty::bool(), &list, Ty::bool(), resolved)
                } else {
                    (&list, &Ty::bool(), resolved, Ty::bool())
                };
                let TypeErrorKind::TyMismatch {
                    expected: found_expected,
                    actual: found_actual,
                } = u.unify_ty(a, b).unwrap_err()
                else {
                    panic!("expected a value type mismatch")
                };
                assert_eq!(found_expected, expected);
                assert_eq!(found_actual, actual);
            }
        }
    }

    #[test]
    fn comp_ty_mismatch_applies_nested_substitutions_without_defaulting_unknowns() {
        for known in [false, true] {
            for reversed in [false, true] {
                let mut u = Unifier::new();
                let payload = u.fresh_ty();
                let grade = u.fresh_grade();
                let result = u.fresh_comp_ty();
                if known {
                    u.unify_ty(&payload, &Ty::bool()).unwrap();
                    let Grade::Var(var) = grade else {
                        unreachable!()
                    };
                    u.bind_grade(var, Grade::Value);
                }
                let producer = CompTy::ret(grade, payload.clone());
                let function = CompTy::fun(Ty::list(payload.clone()), result.clone());
                let resolved_payload = if known { Ty::bool() } else { payload };
                let resolved_grade = if known { Grade::Value } else { grade };
                let resolved_producer = CompTy::ret(resolved_grade, resolved_payload.clone());
                let resolved_function = CompTy::fun(Ty::list(resolved_payload), result);
                let (a, b, expected, actual) = if reversed {
                    (&function, &producer, resolved_function, resolved_producer)
                } else {
                    (&producer, &function, resolved_producer, resolved_function)
                };
                let TypeErrorKind::CompTyMismatch {
                    expected: found_expected,
                    actual: found_actual,
                } = u.unify_comp_ty(a, b).unwrap_err()
                else {
                    panic!("expected a computation type mismatch")
                };
                assert_eq!(found_expected, expected);
                assert_eq!(found_actual, actual);
            }
        }
    }

    #[test]
    fn ty_mismatch_keeps_productive_cycles_finite() {
        let mut u = Unifier::new();
        let recursive = u.fresh_ty();
        let shape = Ty::list(recursive.clone());
        u.unify_ty(&recursive, &shape).unwrap();
        let TypeErrorKind::TyMismatch { expected, actual } =
            u.unify_ty(&shape, &Ty::bool()).unwrap_err()
        else {
            panic!("expected a value type mismatch")
        };
        assert_eq!(expected, Ty::list(Ty::list(recursive)));
        assert_eq!(actual, Ty::bool());
    }

    #[test]
    fn failed_unit_payload_equations_do_not_alias_producer_grades() {
        for handles in [false, true] {
            for reversed in [false, true] {
                let mut u = Unifier::new();
                let (unit_grade, int_grade) = (u.fresh_grade(), u.fresh_grade());
                let (unit, int) = ((unit_grade, &Ty::unit()), (int_grade, &Ty::int()));
                let (a, b) = if reversed { (int, unit) } else { (unit, int) };
                let err = unify_test_producers(&mut u, handles, a, b).unwrap_err();
                if handles {
                    assert!(matches!(err, TypeErrorKind::TyMismatch { .. }));
                } else {
                    assert!(matches!(err, TypeErrorKind::CompTyMismatch { .. }));
                }
                assert_eq!(u.resolve_grade(unit_grade), unit_grade);
                assert_eq!(u.resolve_grade(int_grade), int_grade);

                // Continue after the error: only the Unit producer may become
                // output. The rejected alias must not poison the Int producer.
                unify_test_producers(&mut u, handles, unit, (Grade::Output, &Ty::unit()))
                    .expect("Unit can produce output");
                assert_eq!(u.resolve_grade(unit_grade), Grade::Output);
                assert_eq!(u.resolve_grade(int_grade), int_grade);
                assert!(
                    unify_test_producers(&mut u, handles, int, (Grade::Output, &Ty::unit()))
                        .is_err()
                );
                assert_eq!(u.resolve_grade(int_grade), int_grade);
            }
        }
    }

    #[test]
    fn failed_value_payload_equations_do_not_fix_a_unit_producers_grade() {
        for handles in [false, true] {
            for reversed in [false, true] {
                for improved in [false, true] {
                    let mut u = Unifier::new();
                    let int_grade = if improved {
                        u.fresh_grade()
                    } else {
                        Grade::Value
                    };
                    let unit_grade = u.fresh_grade();
                    let (int, unit) = ((int_grade, &Ty::int()), (unit_grade, &Ty::unit()));
                    if improved {
                        unify_test_producers(&mut u, handles, int, int).unwrap();
                        improve(&mut u);
                    }
                    assert_eq!(u.resolve_grade(int_grade), Grade::Value);
                    let (a, b) = if reversed { (unit, int) } else { (int, unit) };
                    assert!(unify_test_producers(&mut u, handles, a, b).is_err());
                    improve(&mut u);
                    assert_eq!(u.resolve_grade(int_grade), Grade::Value);
                    assert_eq!(u.resolve_grade(unit_grade), unit_grade);

                    unify_test_producers(&mut u, handles, unit, (Grade::Output, &Ty::unit()))
                        .expect("a rejected payload equation leaves Unit able to produce output");
                    assert_eq!(u.resolve_grade(unit_grade), Grade::Output);
                    assert_eq!(u.resolve_grade(int_grade), Grade::Value);
                }
            }
        }
    }

    #[test]
    fn incompatible_fixed_producer_grades_do_not_unify_payloads() {
        for handles in [false, true] {
            for reversed in [false, true] {
                let mut u = Unifier::new();
                let payload = u.fresh_ty();
                let (value, output) = ((Grade::Value, &payload), (Grade::Output, &Ty::unit()));
                let (a, b) = if reversed {
                    (output, value)
                } else {
                    (value, output)
                };
                assert!(unify_test_producers(&mut u, handles, a, b).is_err());
                assert_eq!(u.resolve_ty(&payload), payload);
                u.unify_ty(&payload, &Ty::int())
                    .expect("incompatible fixed grades must refuse before binding the payload");
            }
        }
    }

    #[test]
    fn successful_value_payload_equations_still_fix_free_producer_grades() {
        for handles in [false, true] {
            for reversed in [false, true] {
                let mut u = Unifier::new();
                let payload = u.fresh_ty();
                let grade = u.fresh_grade();
                let (value, free) = ((Grade::Value, &Ty::int()), (grade, &payload));
                let (a, b) = if reversed {
                    (free, value)
                } else {
                    (value, free)
                };
                unify_test_producers(&mut u, handles, a, b)
                    .expect("a successful payload equation may commit the grade equality");
                assert_eq!(u.resolve_ty(&payload), Ty::int());
                assert_eq!(u.resolve_grade(grade), Grade::Value);
            }
        }
    }

    #[test]
    fn failed_deep_data_payload_equations_do_not_alias_producer_grades() {
        for handles in [false, true] {
            for reversed in [false, true] {
                let mut u = Unifier::new();
                let data = u.fresh_kinded(Kind::DATA);
                let record = Ty::record(record_row(&[("fn", Ty::thunk(CompTy::pure(Ty::unit())))]));
                assert!(Kind::DATA.admits(Head::Unit));
                assert!(Kind::DATA.admits(Head::Record));
                let (data_grade, record_grade) = (u.fresh_grade(), u.fresh_grade());
                let (data_pair, record_pair) = ((data_grade, &data), (record_grade, &record));
                let (a, b) = if reversed {
                    (record_pair, data_pair)
                } else {
                    (data_pair, record_pair)
                };
                let (found, kind) = kind_refusal(unify_test_producers(&mut u, handles, a, b));
                assert_eq!(kind, Kind::DATA);
                assert!(
                    matches!(found, KindFound::Type(ty) if matches!(ty.node(), TyNode::Thunk(_)))
                );
                assert_eq!(kind_of(&u, &data), Kind::DATA);
                assert_eq!(u.resolve_grade(data_grade), data_grade);
                assert_eq!(u.resolve_grade(record_grade), record_grade);

                unify_test_producers(&mut u, handles, data_pair, (Grade::Output, &Ty::unit()))
                    .expect("the refused DATA binding still admits Unit");
                assert_eq!(u.resolve_ty(&data), Ty::unit());
                assert_eq!(u.resolve_grade(data_grade), Grade::Output);
                assert_eq!(u.resolve_grade(record_grade), record_grade);
                assert!(
                    unify_test_producers(
                        &mut u,
                        handles,
                        record_pair,
                        (Grade::Output, &Ty::unit())
                    )
                    .is_err()
                );
                assert_eq!(u.resolve_grade(record_grade), record_grade);
            }
        }
    }

    #[test]
    fn successful_unit_capability_narrowing_still_shares_producer_grades() {
        for handles in [false, true] {
            for reversed in [false, true] {
                let mut u = Unifier::new();
                let (data, number) = (u.fresh_kinded(Kind::DATA), u.fresh_kinded(Kind::NUMBER));
                let (data_grade, number_grade) = (u.fresh_grade(), u.fresh_grade());
                let (data_pair, number_pair) = ((data_grade, &data), (number_grade, &number));
                let (a, b) = if reversed {
                    (number_pair, data_pair)
                } else {
                    (data_pair, number_pair)
                };
                unify_test_producers(&mut u, handles, a, b)
                    .expect("numbers satisfy the data obligation");
                assert_eq!(kind_of(&u, &data), Kind::NUMBER);
                assert_eq!(kind_of(&u, &number), Kind::NUMBER);
                assert_eq!(u.resolve_grade(data_grade), u.resolve_grade(number_grade));
                // A payload that excludes Unit improves the shared grade.
                assert_eq!(u.resolve_grade(data_grade), Grade::Value);
                unify_test_producers(&mut u, handles, data_pair, (Grade::Value, &Ty::int()))
                    .expect("both producers can be fixed to return Int");
                assert_eq!(u.resolve_ty(&data), Ty::int());
                assert_eq!(u.resolve_ty(&number), Ty::int());
                assert_eq!(u.resolve_grade(data_grade), Grade::Value);
                assert_eq!(u.resolve_grade(number_grade), Grade::Value);
            }
        }
    }

    #[test]
    fn cyclic_payload_refusal_does_not_alias_producer_grades() {
        for handles in [false, true] {
            for reversed in [false, true] {
                let mut u = Unifier::new();
                let var = u.fresh_ty();
                let cycle = Ty::thunk(CompTy::pure(var.clone()));
                let (var_grade, cycle_grade) = (u.fresh_grade(), u.fresh_grade());
                let (var_pair, cycle_pair) = ((var_grade, &var), (cycle_grade, &cycle));
                let (a, b) = if reversed {
                    (cycle_pair, var_pair)
                } else {
                    (var_pair, cycle_pair)
                };
                assert!(matches!(
                    unify_test_producers(&mut u, handles, a, b),
                    Err(TypeErrorKind::CyclicType { .. })
                ));
                assert_eq!(u.resolve_ty(&var), var);
                assert_eq!(u.resolve_grade(var_grade), var_grade);
                assert_eq!(u.resolve_grade(cycle_grade), cycle_grade);
                unify_test_producers(&mut u, handles, var_pair, (Grade::Output, &Ty::unit()))
                    .expect("the refused cycle leaves its variable free");
                assert_eq!(u.resolve_grade(var_grade), Grade::Output);
                assert_eq!(u.resolve_grade(cycle_grade), cycle_grade);
            }
        }
    }

    #[test]
    fn depth_refusal_does_not_bind_or_alias_a_unit_capable_producers_grade() {
        for handles in [false, true] {
            for reversed in [false, true] {
                for fixed_output in [false, true] {
                    let mut u = Unifier::new();
                    let data = u.fresh_kinded(Kind::DATA);
                    let data_grade = u.fresh_grade();
                    let unit_grade = if fixed_output {
                        Grade::Output
                    } else {
                        u.fresh_grade()
                    };
                    let (data_pair, unit_pair) = ((data_grade, &data), (unit_grade, &Ty::unit()));
                    let (a, b) = if reversed {
                        (unit_pair, data_pair)
                    } else {
                        (data_pair, unit_pair)
                    };
                    let mut pairs = Pairs::default();
                    // Enter the public pair's structural arm with one level
                    // left; DATA admission itself must report TypeTooDeep.
                    let depth = MAX_UNIFY_DEPTH - 1;
                    let result = if handles {
                        u.unify_ty_inner(
                            &Ty::handle(a.0, a.1.clone()),
                            &Ty::handle(b.0, b.1.clone()),
                            &mut pairs,
                            depth,
                        )
                    } else {
                        u.unify_comp_ty_inner(
                            &CompTy::ret(a.0, a.1.clone()),
                            &CompTy::ret(b.0, b.1.clone()),
                            &mut pairs,
                            depth,
                        )
                    };
                    assert!(matches!(result, Err(TypeErrorKind::TypeTooDeep)));
                    assert_eq!(kind_of(&u, &data), Kind::DATA);
                    assert_eq!(u.resolve_grade(data_grade), data_grade);
                    assert_eq!(u.resolve_grade(unit_grade), unit_grade);

                    u.unify_ty(&data, &Ty::int())
                        .expect("failed Unit admission leaves DATA free");
                    unify_test_producers(&mut u, handles, unit_pair, (Grade::Output, &Ty::unit()))
                        .expect("the other Unit producer can still become output");
                    assert_eq!(u.resolve_grade(unit_grade), Grade::Output);
                    assert_eq!(u.resolve_grade(data_grade), data_grade);
                    assert_eq!(u.resolve_ty(&data), Ty::int());
                    assert!(
                        unify_test_producers(
                            &mut u,
                            handles,
                            data_pair,
                            (Grade::Output, &Ty::unit()),
                        )
                        .is_err()
                    );
                    assert_eq!(u.resolve_grade(data_grade), data_grade);
                }
            }
        }
    }

    /// ``Variant{`more: {head: String, tail: Thunk(tail)}, `done: Unit}``.
    fn step(tail: CompTy) -> Ty {
        let payload = Ty::record(Row::extend(
            Label::Field("head".into()),
            Ty::string(),
            Row::extend(Label::Field("tail".into()), Ty::thunk(tail), Row::empty()),
        ));
        Ty::variant(Row::extend(
            Label::Case("more".into()),
            payload,
            Row::extend(Label::Case("done".into()), Ty::unit(), Row::empty()),
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
        let t_body = step(CompTy::pure(Ty::var(t)));
        u.bind_ty_root(t_root, t_body);

        let CompTyNode::Var(c_root) = *u.fresh_comp_ty().node() else {
            unreachable!("fresh_comp_ty yields a Var")
        };
        let step_c = step(CompTy::var(c_root));
        u.bind_comp_root(c_root, CompTy::pure(step_c.clone()));

        u.unify_ty(&Ty::var(t), &step_c)
            .expect("equi-recursive stream types unify regardless of anchor");
    }

    /// `α ≐ {α → Returns Unit}`: a function taking itself.
    #[test]
    fn a_cycle_through_arrows_alone_is_refused() {
        let mut u = Unifier::new();
        let t = u.fresh_tyvar();
        let body = Ty::thunk(CompTy::fun(Ty::var(t), CompTy::pure(Ty::unit())));
        let err = u
            .unify_ty(&Ty::var(t), &body)
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

    #[test]
    fn a_thunk_returning_its_function_keeps_the_outer_argument_cycle_path() {
        for nested_function in [false, true] {
            for guarded in [false, true] {
                let mut u = Unifier::new();
                let t = u.fresh_tyvar();
                let payload = if guarded {
                    Ty::list(Ty::var(t))
                } else {
                    Ty::var(t)
                };
                let mut argument = CompTy::pure(payload);
                if nested_function {
                    argument = CompTy::arrows([Ty::unit()], argument);
                }
                let body = Ty::thunk(CompTy::arrows(
                    [Ty::thunk(argument)],
                    CompTy::pure(Ty::unit()),
                ));
                let result = u.unify_ty(&Ty::var(t), &body);
                if guarded {
                    result.expect("a data guard still admits the recursive type");
                } else {
                    assert!(matches!(
                        result,
                        Err(TypeErrorKind::CyclicType {
                            via: CycleVia::Argument
                        })
                    ));
                }
            }
        }
    }

    #[test]
    fn argument_aliases_do_not_hide_thunks_returning_the_function() {
        for aliased in [false, true] {
            for wrappers in 0..=2 {
                let mut u = Unifier::new();
                let head = u.fresh_comp_ty();
                let mut argument = Ty::thunk(head.clone());
                for _ in 0..wrappers {
                    argument = Ty::thunk(CompTy::pure(argument));
                }
                if aliased {
                    argument = u.fresh_alias(&argument);
                }
                let body = CompTy::arrows([argument], CompTy::pure(Ty::unit()));
                let expected = if wrappers == 0 {
                    CycleVia::Applied
                } else {
                    CycleVia::Argument
                };
                assert!(matches!(
                    u.unify_comp_ty(&head, &body),
                    Err(TypeErrorKind::CyclicType { via }) if via == expected
                ));
            }
        }
    }

    /// `γ ≐ Returns {γ}`: a computation returning a thunk of itself.
    #[test]
    fn a_computation_returning_itself_is_refused() {
        let mut u = Unifier::new();
        let c = u.fresh_comp_ty();
        let body = CompTy::pure(Ty::thunk(c.clone()));
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
        let body = Ty::list(Ty::thunk(CompTy::fun(Ty::var(t), CompTy::pure(Ty::unit()))));
        u.unify_ty(&Ty::var(t), &body)
            .expect("a cycle through a list is a legitimate recursive type");
    }

    // Equal types are one allocation, and unification stops at `a == b`: a test
    // meant to exercise a walk must build two structurally different types.

    /// A `List(List(… foot …))` spine.
    fn deep_list_over(n: u32, foot: Ty) -> Ty {
        (0..n).fold(foot, |ty, _| Ty::list(ty))
    }

    /// A `List(List(… Int …))` spine: no variable, so nothing to memoize.
    fn deep_list(n: u32) -> Ty {
        deep_list_over(n, Ty::int())
    }

    /// A `Ty` deep enough to trip the bound is also deep enough that building
    /// and recursively `Drop`ping the `Arc` chain — nothing to do with the
    /// unifier — nears the default 2 MiB test-thread ceiling.
    fn on_deep_stack(body: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(body)
            .expect("spawn deep-stack thread")
            .join()
            .expect("deep-stack assertion");
    }

    /// A deep variable-free type against one that differs only at the bottom
    /// drives the `List` arm over: the walk must descend past the ceiling to
    /// find the difference.
    #[test]
    fn deeply_nested_type_is_too_deep_not_a_stack_overflow() {
        on_deep_stack(|| {
            let mut u = Unifier::new();
            let a = deep_list(MAX_UNIFY_DEPTH + 100);
            let b = deep_list_over(MAX_UNIFY_DEPTH + 100, Ty::bool());
            let err = u
                .unify_ty(&a, &b)
                .expect_err("a type nested past the ceiling must not unify");
            assert!(
                matches!(err, TypeErrorKind::TypeTooDeep),
                "expected TypeTooDeep, got {err:?}"
            );
        });
    }

    #[test]
    fn deeply_nested_data_binds_only_through_representation_only_aliases() {
        on_deep_stack(|| {
            let mut u = Unifier::new();
            let deep = deep_list(MAX_UNIFY_DEPTH + 100);
            let alias = u.fresh_alias(&deep);
            assert_eq!(u.resolve_ty(&alias), deep);
            let checked = u.fresh_ty();
            assert!(matches!(
                u.unify_ty(&checked, &deep),
                Err(TypeErrorKind::TypeTooDeep),
            ));
            assert_eq!(u.resolve_ty(&checked), checked);
        });
    }

    #[test]
    fn checked_binding_counts_arrow_arguments_but_not_the_curried_spine() {
        on_deep_stack(|| {
            let nested_arrows = (0..MAX_UNIFY_DEPTH + 100).fold(Ty::int(), |arg, _| {
                Ty::thunk(CompTy::arrows([arg], CompTy::pure(Ty::unit())))
            });
            for (arg, deep_argument) in [
                (Ty::int(), false),
                (deep_list(MAX_UNIFY_DEPTH + 100), true),
                (nested_arrows, true),
            ] {
                let mut u = Unifier::new();
                let body = CompTy::arrows(
                    std::iter::once(arg).chain((0..2_000).map(|_| Ty::int())),
                    CompTy::pure(Ty::int()),
                );
                let variable = u.fresh_comp_ty();
                let result = u.unify_comp_ty(&variable, &body);
                if deep_argument {
                    assert!(matches!(result, Err(TypeErrorKind::TypeTooDeep)));
                } else {
                    result.expect("a curried spine is width in a fresh binding");
                }
            }
        });
    }

    #[test]
    fn checked_binding_counts_record_payloads_but_not_the_row_spine() {
        on_deep_stack(|| {
            for deep_payload in [false, true] {
                let mut u = Unifier::new();
                let payload = if deep_payload {
                    deep_list(MAX_UNIFY_DEPTH + 100)
                } else {
                    Ty::int()
                };
                let row = (0..2_000).fold(Row::empty(), |rest, n| {
                    Row::extend(Label::Field(format!("field{n}")), Ty::int(), rest)
                });
                let ty = Ty::record(Row::extend(Label::Field("payload".into()), payload, row));
                let variable = u.fresh_ty();
                let result = u.unify_ty(&variable, &ty);
                if deep_payload {
                    assert!(matches!(result, Err(TypeErrorKind::TypeTooDeep)));
                } else {
                    result.expect("a row spine is width in a fresh binding");
                }
            }
        });
    }

    /// An already-bound ty-var meets a structure as a one-sided obligation.
    /// The structure ends in a variable: a ground one is its own shallow term.
    #[test]
    fn deeply_nested_one_sided_obligation_is_too_deep_not_a_stack_overflow() {
        on_deep_stack(|| {
            let mut u = Unifier::new();
            let root = u.fresh_tyvar();
            let foot = u.fresh_ty();
            let deep = deep_list_over(MAX_UNIFY_DEPTH + 100, foot);
            u.bind_ty_root(root, deep.clone());
            let err = u
                .unify_ty(&Ty::var(root), &deep)
                .expect_err("the shallow term of a too-deep type must report TypeTooDeep");
            assert!(
                matches!(err, TypeErrorKind::TypeTooDeep),
                "expected TypeTooDeep, got {err:?}"
            );
        });
    }

    /// Check payload syntax before aliases can hide its nesting.
    #[test]
    fn deeply_nested_field_type_in_row_binding_is_too_deep() {
        on_deep_stack(|| {
            let mut u = Unifier::new();
            let rho = u.fresh_row_var();
            let row = Row::extend(
                Label::Field("x".into()),
                deep_list(MAX_UNIFY_DEPTH + 100),
                Row::empty(),
            );
            let slots = u.tys.slots.len();
            let err = u
                .unify_row(&Row::var(rho), &row)
                .expect_err("a too-deep field type must report TypeTooDeep before aliasing");
            assert_eq!(u.tys.slots.len(), slots);
            assert!(u.rows.get_ref(rho).is_none());
            assert!(
                matches!(err, TypeErrorKind::TypeTooDeep),
                "expected TypeTooDeep, got {err:?}"
            );
        });
    }

    fn record_row(fields: &[(&str, Ty)]) -> Row {
        fields.iter().rev().fold(Row::empty(), |rest, (l, t)| {
            Row::extend(Label::Field((*l).into()), t.clone(), rest)
        })
    }

    fn open_row(fields: &[(&str, Ty)], tail: RowVar) -> Row {
        fields.iter().rev().fold(Row::var(tail), |rest, (l, t)| {
            Row::extend(Label::Field((*l).into()), t.clone(), rest)
        })
    }

    fn resolved_fields(u: &Unifier, row: &Row) -> std::collections::HashMap<String, Ty> {
        let mut out = std::collections::HashMap::new();
        let mut cur = u.apply_row(row);
        while let RowNode::Extend(l, t, rest) = cur.node() {
            out.insert(l.to_string(), t.clone());
            cur = rest.clone();
        }
        out
    }

    /// The no-false-positive half of the bound: a record far wider than
    /// [`MAX_UNIFY_DEPTH`] but shallow must still unify.  Its twin ends in a
    /// variable, so the whole spine is walked.
    #[test]
    fn wide_record_unifies_without_being_too_deep() {
        let width = (MAX_UNIFY_DEPTH as usize) * 4;
        let labels: Vec<String> = (0..width).map(|i| format!("f{i}")).collect();
        let fields: Vec<(&str, Ty)> = labels.iter().map(|l| (l.as_str(), Ty::int())).collect();
        let mut u = Unifier::new();
        let left = record_row(&fields);
        let (last, init) = fields.split_last().unwrap();
        let mut twin = init.to_vec();
        twin.push((last.0, u.fresh_ty()));
        let right = record_row(&twin);
        u.unify_row(&left, &right)
            .expect("a wide-but-shallow record must unify, not be rejected as too deep");
    }

    /// The same width property for a variant row: alternatives are siblings.
    /// Its twin's last payload is a variable.
    #[test]
    fn wide_variant_unifies_without_being_too_deep() {
        let width = (MAX_UNIFY_DEPTH as usize) * 4;
        let labels: Vec<String> = (0..width).map(|i| format!("`t{i}")).collect();
        let fields: Vec<(&str, Ty)> = labels.iter().map(|l| (l.as_str(), Ty::unit())).collect();
        let mut u = Unifier::new();
        let left = Ty::variant(record_row(&fields));
        let (last, init) = fields.split_last().unwrap();
        let mut twin = init.to_vec();
        twin.push((last.0, u.fresh_ty()));
        let right = Ty::variant(record_row(&twin));
        u.unify_ty(&left, &right)
            .expect("a wide-but-shallow variant must unify, not be rejected as too deep");
    }

    /// The rewrite pairs labels regardless of spine position: order is free,
    /// with a shared tail as without, which the divergence guard must let run.
    #[test]
    fn remy_rewrite_under_permutation() {
        for shared in [false, true] {
            let mut u = Unifier::new();
            let tail = shared.then(|| u.fresh_row_var());
            let row = |fields: &[(&str, Ty)]| match tail {
                Some(rho) => open_row(fields, rho),
                None => record_row(fields),
            };
            let (a, b) = (u.fresh_ty(), u.fresh_ty());
            let left = row(&[("a", Ty::int()), ("b", Ty::string())]);
            let right = row(&[("b", b.clone()), ("a", a.clone())]);
            u.unify_row(&left, &right).expect("permuted rows unify");
            assert_eq!(u.apply_ty(&a), Ty::int(), "shared tail: {shared}");
            assert_eq!(u.apply_ty(&b), Ty::string(), "shared tail: {shared}");
        }
    }

    /// An open row against a closed one binds the tail to carry the surplus.
    #[test]
    fn remy_rewrite_binds_open_tail() {
        let mut u = Unifier::new();
        let rho = u.fresh_row_var();
        let open = open_row(&[("a", Ty::int())], rho);
        let closed = record_row(&[("a", Ty::int()), ("b", Ty::string())]);
        u.unify_row(&open, &closed)
            .expect("open row absorbs the extra field");
        let fields = resolved_fields(&u, &Row::var(rho));
        assert_eq!(
            fields.get("b"),
            Some(&Ty::string()),
            "tail absorbed the `b` field"
        );
    }

    /// Which record lacks the label must not depend on the order a literal
    /// wrote its fields in: the Rémy rewrite keeps each side on its side.
    #[test]
    fn a_missing_field_is_reported_whatever_the_label_order() {
        let closed = |fields: &[&str]| {
            fields.iter().rev().fold(Row::empty(), |rest, l| {
                Row::extend(Label::Field((*l).into()), Ty::int(), rest)
            })
        };
        let wanted = closed(&["p", "q", "r"]);
        for written in [["p", "q"], ["q", "p"]] {
            let err = Unifier::new()
                .unify_row(&closed(&written), &wanted)
                .expect_err("a record without `r` must not unify");
            assert!(
                matches!(&err, TypeErrorKind::RowExtraField { label, .. } if label.name() == "r"),
                "{written:?}: expected RowExtraField(r), got {err:?}"
            );
        }
    }

    /// `{x: Int | ρ} ≐ {y: Int | ρ}` has no solution; report, do not diverge.
    #[test]
    fn shared_tail_mismatched_heads_is_recursive_row() {
        let mut u = Unifier::new();
        let rho = u.fresh_row_var();
        let left = open_row(&[("x", Ty::int())], rho);
        let right = open_row(&[("y", Ty::int())], rho);
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
        let left = open_row(&[("x", Ty::int()), ("a", Ty::int())], rho);
        let right = open_row(&[("y", Ty::int()), ("b", Ty::int())], rho);
        let err = u
            .unify_row(&left, &right)
            .expect_err("shared-tail deep mismatch must not unify");
        assert!(
            matches!(err, TypeErrorKind::RecursiveRow),
            "expected RecursiveRow, got {err:?}"
        );
    }

    #[test]
    fn recursive_value_field_keeps_a_finite_row_spine() {
        let mut u = Unifier::new();
        let value = u.fresh_ty();
        let tail = u.fresh_row_var();
        let record = Ty::record(open_row(&[("a", value.clone())], tail));
        u.unify_ty(&value, &record)
            .expect("recursion through a value field is productive data");
        u.unify_row(&Row::var(tail), &record_row(&[("b", Ty::int())]))
            .expect("the row tail remains a finite field list");
    }

    #[test]
    fn cycle_through_field_type_keeps_a_finite_row_and_round_trips() {
        use super::super::generalize::{generalize, instantiate, settle_weak};
        use crate::ty::{Site, UnitId};

        for with_n in [false, true] {
            let mut u = Unifier::new();
            u.enter();
            let rho = u.fresh_row_var();
            let prefix = if with_n {
                vec![("n", Ty::int())]
            } else {
                vec![]
            };
            let inner = Ty::record(open_row(&prefix, rho));
            u.unify_row(&Row::var(rho), &record_row(&[("x", inner.clone())]))
                .unwrap();
            let (fields, rest) = row_parts(&u, &Row::var(rho));
            assert_eq!(rest, Row::empty());
            assert_eq!(fields.len(), 1);
            assert!(
                matches!(fields[0].1.node(), TyNode::Var(_)),
                "stored payload is anchored"
            );
            let expected = u.fresh_ty();
            let mut expected_fields = prefix;
            expected_fields.push(("x", expected.clone()));
            u.unify_ty(&expected, &Ty::record(record_row(&expected_fields)))
                .unwrap();
            u.unify_ty(&inner, &expected).unwrap();
            u.exit();
            let scheme = settle_weak(&u, Arc::new(generalize(&u, &inner)));
            assert!(scheme.tys.values().any(|role| role.mu().is_some()));
            assert!(scheme.to_string().contains('μ'));
            let mut next = Unifier::new();
            let imported = instantiate(&mut next, &scheme);
            let site = Site::snapshot(&next, &imported, UnitId::fresh());
            let round_trip = site.instantiate(&mut next);
            next.unify_ty(&imported, &round_trip).unwrap();
            assert_ne!(next.cyclic_roots_in_ty(&round_trip).1, []);
        }
    }

    #[test]
    fn row_payload_anchors_preserve_syntax_variables_and_stored_tails() {
        let mut u = Unifier::new();
        let existing = u.fresh_kinded(Kind::COMPARABLE);
        let tail = u.fresh_row_var();
        u.unify_row(&Row::var(tail), &record_row(&[("last", Ty::bool())]))
            .unwrap();
        let stored_tail = u.rows.get_ref(tail).unwrap().clone();
        let owner = u.fresh_row_var();
        let proposed = open_row(
            &[
                ("x", Ty::int()),
                ("x", Ty::string()),
                ("same", existing.clone()),
            ],
            tail,
        );
        u.unify_row(&Row::var(owner), &proposed).unwrap();
        let mut stored = u.rows.get_ref(owner).unwrap();
        for (label, expected) in [
            ("x", Ty::int()),
            ("x", Ty::string()),
            ("same", existing.clone()),
        ] {
            let RowNode::Extend(l, payload, rest) = stored.node() else {
                panic!("stored field")
            };
            assert_eq!(l.name(), label);
            assert!(matches!(payload.node(), TyNode::Var(_)));
            assert_eq!(u.resolve_ty(payload), expected);
            if label == "same" {
                assert_eq!(*payload, existing);
            }
            stored = rest;
        }
        assert_eq!(*stored, Row::var(tail));
        assert_eq!(*u.rows.get_ref(tail).unwrap(), stored_tail);
    }

    #[test]
    fn mutual_and_guarded_row_payload_cycles_are_productive() {
        for guard in 0..5 {
            for reverse in [false, true] {
                let mut u = Unifier::new();
                let rho = u.fresh_row_var();
                let sigma = u.fresh_row_var();
                let wrap = |row: Row| match guard {
                    0 => Ty::record(row),
                    1 => Ty::variant(Row::extend(
                        Label::Case("next".into()),
                        Ty::record(row),
                        Row::empty(),
                    )),
                    2 => Ty::list(Ty::record(row)),
                    3 => Ty::map(Ty::record(row)),
                    _ => Ty::thunk(CompTy::pure(Ty::record(row))),
                };
                let first = record_row(&[("next", wrap(Row::var(sigma)))]);
                let second = record_row(&[("back", wrap(Row::var(rho)))]);
                let equations = [(rho, &first), (sigma, &second)];
                for index in if reverse { [1, 0] } else { [0, 1] } {
                    let (owner, row) = equations[index];
                    u.unify_row(&Row::var(owner), row).unwrap();
                }
                let ty = Ty::record(Row::var(rho));
                assert_ne!(u.cyclic_roots_in_ty(&ty).1, []);
                let _ = u.apply_ty(&ty);
                let (fields, tail) = row_parts(&u, &Row::var(rho));
                assert_eq!(tail, Row::empty());
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].0.name(), "next");
                assert!(matches!(fields[0].1.node(), TyNode::Var(_)));
                let expected = u.fresh_ty();
                let unfolded = Ty::record(record_row(&[(
                    "next",
                    wrap(record_row(&[("back", wrap(Row::var(rho)))])),
                )]));
                u.unify_ty(&expected, &unfolded).unwrap();
                u.unify_ty(&ty, &expected).unwrap();
            }
        }
        // A variant row can itself be the cycle's row owner.
        let mut u = Unifier::new();
        let rho = u.fresh_row_var();
        let row = Row::extend(
            Label::Case("more".into()),
            Ty::variant(Row::var(rho)),
            Row::empty(),
        );
        u.unify_row(&Row::var(rho), &row).unwrap();
        assert_ne!(u.cyclic_roots_in_ty(&Ty::variant(Row::var(rho))).1, []);
    }

    #[test]
    fn infinite_row_spines_and_same_tail_duplicate_mismatches_stay_rejected() {
        for reverse in [false, true] {
            for aliased in [false, true] {
                let mut u = Unifier::new();
                let rho = u.fresh_row_var();
                let alias = u.fresh_row_var();
                if aliased {
                    u.unify_row(&Row::var(alias), &Row::var(rho)).unwrap();
                }
                let tail = if aliased { alias } else { rho };
                let growing = open_row(&[("a", Ty::int())], tail);
                let result = if reverse {
                    u.unify_row(&growing, &Row::var(rho))
                } else {
                    u.unify_row(&Row::var(rho), &growing)
                };
                assert!(matches!(result, Err(TypeErrorKind::RecursiveRow)));
            }
            let mut u = Unifier::new();
            let rho = u.fresh_row_var();
            let sigma = u.fresh_row_var();
            u.unify_row(&Row::var(rho), &open_row(&[("a", Ty::int())], sigma))
                .unwrap();
            let growing = open_row(&[("b", Ty::string())], rho);
            let result = if reverse {
                u.unify_row(&growing, &Row::var(sigma))
            } else {
                u.unify_row(&Row::var(sigma), &growing)
            };
            assert!(matches!(result, Err(TypeErrorKind::RecursiveRow)));
            for names in [["a", "b"], ["a", "a"]] {
                let mut u = Unifier::new();
                let tail = u.fresh_row_var();
                let left = open_row(
                    &[("b", Ty::int()), ("a", Ty::int()), ("a", Ty::int())],
                    tail,
                );
                let right = open_row(&[(names[0], Ty::int()), (names[1], Ty::int())], tail);
                let result = if reverse {
                    u.unify_row(&right, &left)
                } else {
                    u.unify_row(&left, &right)
                };
                assert!(matches!(result, Err(TypeErrorKind::RecursiveRow)));
            }
        }
    }

    #[test]
    fn recursive_row_equality_does_not_hide_rigid_payload_or_closed_field_clashes() {
        for one_sided in [false, true] {
            for reverse in [false, true] {
                for missing in [false, true] {
                    let mut u = Unifier::new();
                    let rho = u.fresh_row_var();
                    let left = Ty::record(open_row(&[("rigid", Ty::int())], rho));
                    u.unify_row(&Row::var(rho), &record_row(&[("next", left.clone())]))
                        .unwrap();
                    let root = u.fresh_ty();
                    let mut fields = vec![("next", root.clone())];
                    if !missing {
                        fields.push(("rigid", Ty::string()));
                    }
                    let structure = Ty::record(record_row(&fields));
                    u.unify_ty(&root, &structure).unwrap();
                    let right = if one_sided { structure } else { root };
                    let result = if reverse {
                        u.unify_ty(&right, &left)
                    } else {
                        u.unify_ty(&left, &right)
                    };
                    assert!(
                        result.is_err(),
                        "one-sided={one_sided}, reverse={reverse}, missing={missing}"
                    );
                    assert!(!matches!(
                        result,
                        Err(TypeErrorKind::RecursiveRow | TypeErrorKind::TypeTooDeep)
                    ));
                }
            }
        }
    }

    #[test]
    fn deep_row_payload_cycles_check_before_installation_and_keep_later_obligations() {
        for later in [false, true] {
            for handle in [false, true] {
                let mut u = Unifier::new();
                let rho = u.fresh_deep_row_var(true);
                let payload = u.fresh_ty();
                let forbidden = if handle {
                    Ty::handle(Grade::Value, Ty::int())
                } else {
                    Ty::thunk(CompTy::pure(Ty::int()))
                };
                let inner = Ty::record(open_row(
                    &[(
                        "data",
                        if later {
                            payload.clone()
                        } else {
                            forbidden.clone()
                        },
                    )],
                    rho,
                ));
                let proposed = record_row(&[("next", inner)]);
                let slots = u.tys.slots.len();
                let result = u.unify_row(&Row::var(rho), &proposed);
                if later {
                    result.unwrap();
                    assert_eq!(kind_of(&u, &payload), Kind::DATA);
                    assert!(matches!(
                        u.unify_ty(&payload, &forbidden),
                        Err(TypeErrorKind::KindMismatch { .. })
                    ));
                    u.unify_ty(&payload, &Ty::int()).unwrap();
                    let _ = u.apply_ty(&Ty::record(Row::var(rho)));
                } else {
                    assert!(matches!(result, Err(TypeErrorKind::KindMismatch { .. })));
                    assert_eq!(u.tys.slots.len(), slots, "no alias before validation");
                    assert!(u.rows.get_ref(rho).is_none());
                }
            }
        }
    }

    #[test]
    fn row_payload_aliases_lower_rank_weak_sources_and_register_producers() {
        for rank in [0, 1] {
            for weak in [false, true] {
                let mut u = Unifier::new();
                for _ in 0..rank {
                    u.enter();
                }
                let rho = u.fresh_row_var();
                let source = WeakSource::Boundary("from-json".into());
                if weak {
                    u.mark_weak(&Ty::record(Row::var(rho)), &source);
                }
                u.enter();
                let payload = u.fresh_ty();
                let grade = u.fresh_grade();
                let inner = Ty::thunk(CompTy::ret(grade, payload.clone()));
                let proposed =
                    record_row(&[("next", Ty::record(open_row(&[("producer", inner)], rho)))]);
                u.unify_row(&Row::var(rho), &proposed).unwrap();
                let expected_rank = if weak { 0 } else { rank };
                let mut reachable = Vec::new();
                let _ = u.reach(
                    Term::Row(&Row::var(rho)),
                    &mut Visited::default(),
                    &mut |var| {
                        reachable.push(var);
                        ControlFlow::<()>::Continue(())
                    },
                );
                assert!(reachable.iter().filter(|v| matches!(v, Var::Ty(_))).count() >= 2);
                for var in reachable {
                    assert_eq!(u.rank_of(var), expected_rank);
                    if weak && !matches!(var, Var::Grade(_)) {
                        assert_eq!(u.weak_source_of(var), Some(&source));
                    }
                }
                u.unify_ty(&payload, &Ty::int()).unwrap();
                improve(&mut u);
                assert_eq!(u.resolve_grade(grade), Grade::Value);
            }
        }
    }

    /// A probe against a duplicated-label spine binds to the *head* occurrence,
    /// since row unification walks head-first.  `infer_map_val` dedups last-wins
    /// upstream, so the shape only reaches the unifier here.
    #[test]
    fn duplicate_label_spine_matches_head() {
        let mut u = Unifier::new();
        let alpha = u.fresh_ty();
        let rho = u.fresh_row_var();
        let dup = record_row(&[("x", Ty::int()), ("x", Ty::string())]);
        let probe = open_row(&[("x", alpha.clone())], rho);
        u.unify_row(&probe, &dup)
            .expect("probe unifies against a duplicated-label spine");
        assert_eq!(
            u.apply_ty(&alpha),
            Ty::int(),
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
        let TyNode::Var(v) = *u.resolve_ty(ty).node() else {
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
        u.unify_ty(&a, &Ty::int()).expect("Int is a number");
        assert_eq!(u.resolve_ty(&b), Ty::int());
    }

    #[test]
    fn a_meet_with_one_nullary_head_binds_the_variable() {
        let mut u = Unifier::new();
        let a = u.fresh_kinded(Kind::SIZED);
        let b = u.fresh_kinded(Kind::COMPARABLE);
        u.unify_ty(&a, &b).expect("text is both");
        assert_eq!(u.resolve_ty(&a), Ty::string());
        assert_eq!(u.resolve_ty(&b), Ty::string());
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
        let (found, kind) = kind_refusal(u.unify_ty(&a, &Ty::string()));
        assert!(matches!(found, KindFound::Type(ty) if ty == Ty::string()));
        assert_eq!(kind, Kind::NUMBER);
        assert_eq!(kind_of(&u, &a), Kind::NUMBER);
        u.unify_ty(&a, &Ty::float()).expect("Float is a number");
    }

    #[test]
    fn a_deep_kind_reaches_a_list_element() {
        let mut u = Unifier::new();
        let data = u.fresh_kinded(Kind::DATA);
        let elem = u.fresh_ty();
        u.unify_ty(&data, &Ty::list(elem.clone()))
            .expect("a list of something");
        assert_eq!(kind_of(&u, &elem), Kind::DATA);
        let block = Ty::thunk(CompTy::pure(Ty::int()));
        let (found, kind) = kind_refusal(u.unify_ty(&elem, &block));
        assert!(matches!(found, KindFound::Type(ty) if matches!(ty.node(), TyNode::Thunk(_))));
        assert_eq!(kind, Kind::DATA);
    }

    #[test]
    fn a_deep_kind_refuses_a_block_in_what_it_is_bound_to() {
        let mut u = Unifier::new();
        let data = u.fresh_kinded(Kind::DATA);
        let block = Ty::thunk(CompTy::pure(Ty::int()));
        kind_refusal(u.unify_ty(&data, &Ty::list(block)));
    }

    #[test]
    fn a_deep_kind_reaches_a_records_fields_and_its_open_tail() {
        let mut u = Unifier::new();
        let data = u.fresh_kinded(Kind::DATA);
        let (field, tail) = (u.fresh_ty(), u.fresh_row_var());
        u.unify_ty(&data, &Ty::record(open_row(&[("a", field.clone())], tail)))
            .expect("a record with a field of something");
        assert_eq!(kind_of(&u, &field), Kind::DATA);
        assert!(u.is_deep_row(tail));

        let block = Ty::thunk(CompTy::pure(Ty::int()));
        let later = record_row(&[("b", block)]);
        kind_refusal(u.unify_row(&Row::var(tail), &later));

        let fine = record_row(&[("b", Ty::int())]);
        u.unify_row(&Row::var(tail), &fine)
            .expect("an Int field is data");
    }

    #[test]
    fn a_deep_kind_reaches_a_variants_payloads() {
        let mut u = Unifier::new();
        let data = u.fresh_kinded(Kind::DATA);
        let (payload, tail) = (u.fresh_ty(), u.fresh_row_var());
        let tag = Row::extend(Label::Case("ok".into()), payload.clone(), Row::var(tail));
        u.unify_ty(&data, &Ty::variant(tag))
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
        u.unify_row(&Row::var(shallow), &Row::var(deep))
            .expect("two row variables unite");
        assert!(u.is_deep_row(shallow) && u.is_deep_row(deep));
    }
}
