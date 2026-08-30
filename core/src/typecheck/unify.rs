//! Union-find unifier over four variable kinds: type, computation type, route, row.
//!
//! Record↔Map is the one coercion the language keeps; payload routes get
//! none, so `Value` and `Bytes` never unify.
//!
//! Value and computation types are both *equi-recursive* — a slot may be bound
//! to a structure containing its own variable — so neither needs an occurs
//! check.  Instead every traversal carries a [`Visited`] of the roots it is
//! expanding, turning a cycle into a back-edge, and unification carries a
//! co-inductive [`Pairs`], so two cyclic types reach a fixed point.

use super::error::{CompDiff, TypeErrorKind};
use super::route::RouteMismatch;
use super::ty::{CompTy, CompTyVar, PayloadRoute, PayloadVar, Row, RowVar, Ty, TyVar};
use crate::syntax::tag::is_tag_label;
use std::collections::HashSet;

/// Cycle-tracking state, threaded through `apply_*` here and `free_*` in
/// `generalize.rs`.  `tys`/`comps` are a stack for `apply_*` and a set for
/// `free_*`, whose collection is idempotent.  A root that proves cyclic goes
/// into `cyclic_*` and stays visited for the rest of the call, so siblings
/// sharing that subtree keep getting back-edges — while a non-cyclic root stays
/// out, letting a sibling `τ → Int` root re-resolve to `Int`.
#[derive(Default)]
pub(super) struct Visited {
    pub tys: HashSet<u32>,
    pub comps: HashSet<u32>,
    cyclic_tys: HashSet<u32>,
    cyclic_comps: HashSet<u32>,
}

/// Equality obligations already in progress; re-entering one is an immediate
/// success, which is what makes two cyclic types terminate.  Alongside the
/// symmetric root pairs are *one-sided* obligations — a root against a
/// [`TyKey`] / [`CompTyKey`] of the other side — because a value-anchored
/// stream (`T = Step(F T)`) meets its comp-anchored twin (`C = F Step(C)`) as
/// `Var`-vs-structure, never as a `Var`/`Var` pair.
#[derive(Default)]
struct Pairs {
    tys: HashSet<(u32, u32)>,
    comps: HashSet<(u32, u32)>,
    ty_expansions: HashSet<(u32, TyKey)>,
    comp_expansions: HashSet<(u32, CompTyKey)>,
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
    Var(u32),
}

/// Fingerprint of a computation type.  See [`TyKey`].
#[derive(Clone, PartialEq, Eq, Hash)]
enum CompTyKey {
    Return(PayloadRoute, Box<TyKey>),
    Fun(Box<TyKey>, Box<Self>),
    Var(u32),
}

/// Fingerprint of a row spine.  See [`TyKey`].
#[derive(Clone, PartialEq, Eq, Hash)]
enum RowKey {
    Empty,
    Var(u32),
    Extend(String, Box<TyKey>, Box<Self>),
}

enum Slot<T> {
    Free,
    Bound(T),
    Parent(u32),
}

struct Store<T> {
    slots: Vec<Slot<T>>,
    next: u32,
}

/// A sort with variables in a union-find store: `as_var` projects the id out of
/// a bare variable, `from_root` rebuilds one at a canonical root.
trait Unifiable: Clone {
    fn as_var(&self) -> Option<u32>;
    fn from_root(root: u32) -> Self;
}

impl Unifiable for Ty {
    fn as_var(&self) -> Option<u32> {
        match self {
            Self::Var(TyVar(i)) => Some(*i),
            _ => None,
        }
    }
    fn from_root(root: u32) -> Self {
        Self::Var(TyVar(root))
    }
}

impl Unifiable for CompTy {
    fn as_var(&self) -> Option<u32> {
        match self {
            Self::Var(CompTyVar(i)) => Some(*i),
            _ => None,
        }
    }
    fn from_root(root: u32) -> Self {
        Self::Var(CompTyVar(root))
    }
}

impl Unifiable for PayloadRoute {
    fn as_var(&self) -> Option<u32> {
        match self {
            Self::Var(PayloadVar(i)) => Some(*i),
            _ => None,
        }
    }
    fn from_root(root: u32) -> Self {
        Self::Var(PayloadVar(root))
    }
}

impl Unifiable for Row {
    fn as_var(&self) -> Option<u32> {
        match self {
            Self::Var(RowVar(i)) => Some(*i),
            _ => None,
        }
    }
    fn from_root(root: u32) -> Self {
        Self::Var(RowVar(root))
    }
}

impl<T: Clone> Store<T> {
    fn new() -> Self {
        Self {
            slots: Vec::new(),
            next: 0,
        }
    }

    fn fresh(&mut self) -> u32 {
        let id = self.next;
        self.next += 1;
        self.slots.push(Slot::Free);
        id
    }

    fn find(&mut self, i: u32) -> u32 {
        // Out-of-range ids belong to a foreign unifier — cached prelude schemes
        // loaded into a fresh `InferCtx`.  Treat them as free.
        if i as usize >= self.slots.len() {
            return i;
        }
        match self.slots[i as usize] {
            Slot::Parent(p) => {
                let r = self.find(p);
                self.slots[i as usize] = Slot::Parent(r);
                r
            }
            _ => i,
        }
    }

    fn get(&mut self, i: u32) -> Option<T> {
        if i as usize >= self.slots.len() {
            return None;
        }
        let r = self.find(i);
        match &self.slots[r as usize] {
            Slot::Bound(t) => Some(t.clone()),
            _ => None,
        }
    }

    /// Grow to cover `i`: cached prelude vars can arrive above a fresh `next`.
    fn ensure(&mut self, i: u32) {
        let needed = (i as usize) + 1;
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

    fn bind(&mut self, i: u32, val: T) {
        self.ensure(i);
        self.slots[i as usize] = Slot::Bound(val);
    }

    fn union(&mut self, a: u32, b: u32) {
        self.ensure(a.max(b));
        self.slots[a as usize] = Slot::Parent(b);
    }

    fn unite(&mut self, a: u32, b: u32) {
        if a == b {
            return;
        }
        let ar = self.find(a);
        let br = self.find(b);
        if ar != br {
            self.union(ar, br);
        }
    }
}

impl<T: Unifiable> Store<T> {
    /// Follow a variable chain to canonical form.  The walk stops at the first
    /// non-variable head, so variables nested inside it — a cyclic binding's
    /// back-edges — survive untouched.
    fn resolve(&mut self, x: &T) -> T {
        match x.as_var() {
            Some(i) => match self.get(i) {
                Some(b) => self.resolve(&b),
                None => T::from_root(self.find(i)),
            },
            None => x.clone(),
        }
    }
}

pub struct Unifier {
    tys: Store<Ty>,
    ctys: Store<CompTy>,
    routes: Store<PayloadRoute>,
    rows: Store<Row>,
}

impl Unifier {
    pub fn new() -> Self {
        Self {
            tys: Store::new(),
            ctys: Store::new(),
            routes: Store::new(),
            rows: Store::new(),
        }
    }

    pub fn fresh_tyvar(&mut self) -> TyVar {
        TyVar(self.tys.fresh())
    }
    pub fn fresh_ty(&mut self) -> Ty {
        Ty::Var(self.fresh_tyvar())
    }

    pub fn fresh_routevar(&mut self) -> PayloadVar {
        PayloadVar(self.routes.fresh())
    }
    /// The unconstrained `F[μ] _`, for a head whose route is not yet known —
    /// a signature nobody declared, so it must constrain nothing.
    pub fn fresh_route(&mut self) -> PayloadRoute {
        PayloadRoute::Var(self.fresh_routevar())
    }

    pub fn fresh_comp_ty(&mut self) -> CompTy {
        CompTy::Var(CompTyVar(self.ctys.fresh()))
    }
    pub fn fresh_row_var(&mut self) -> RowVar {
        RowVar(self.rows.fresh())
    }
    pub fn fresh_row(&mut self) -> Row {
        Row::Var(self.fresh_row_var())
    }

    /// Canonical comp-var root under union-find, for the cycle-aware traversals
    /// in `generalize.rs`.
    pub fn comp_root(&mut self, i: u32) -> u32 {
        self.ctys.find(i)
    }

    /// Canonical ty-var root under union-find.  Mirror of `comp_root`.
    pub fn ty_root(&mut self, i: u32) -> u32 {
        self.tys.find(i)
    }

    /// A fresh comp-var slot, as a root id.  Instantiation mints one per cyclic
    /// comp-var so each use of a recursive scheme gets independent slots.
    pub fn fresh_comp_root(&mut self) -> u32 {
        self.ctys.fresh()
    }

    /// Mirror of `fresh_comp_root` for cyclic ty bindings.
    pub fn fresh_ty_root(&mut self) -> u32 {
        self.tys.fresh()
    }

    /// Pairs with `fresh_comp_root`: the scheme's snapshot, substituted.
    pub fn bind_comp_root(&mut self, root: u32, value: CompTy) {
        self.ctys.bind(root, value);
    }

    /// Mirror of `bind_comp_root` for cyclic ty bindings.
    pub fn bind_ty_root(&mut self, root: u32, value: Ty) {
        self.tys.bind(root, value);
    }

    /// The root's binding with substitutions applied, or `None` if unbound or a
    /// bare `Var`; `generalize` snapshots cyclic bindings this way.  Quote *from
    /// the root*, never the stored body: one level below the anchor unrolls the
    /// cycle before the back-edge fires, so the snapshot comes out off by a
    /// level and leaks the original union-find slot there.
    pub fn resolved_comp_root_binding(&mut self, root: u32) -> Option<CompTy> {
        match self.ctys.get(root) {
            Some(CompTy::Var(_)) | None => None,
            Some(_) => Some(self.apply_comp_ty(&CompTy::Var(CompTyVar(root)))),
        }
    }

    /// Mirror of `resolved_comp_root_binding`; the same anchor-quoting applies.
    pub fn resolved_ty_root_binding(&mut self, root: u32) -> Option<Ty> {
        match self.tys.get(root) {
            Some(Ty::Var(_)) | None => None,
            Some(_) => Some(self.apply_ty(&Ty::Var(TyVar(root)))),
        }
    }

    /// The comp-var and ty-var roots on some cycle reachable from `ty`.  Reads
    /// the traversal's tags, not its output: a mid-cycle root is tagged on
    /// detection but need not surface as a back-edge.  One walk populates both
    /// tag sets, so `generalize` asks once and reads both.
    pub(super) fn cyclic_roots_in_ty(&mut self, ty: &Ty) -> (Vec<u32>, Vec<u32>) {
        let mut visited = Visited::default();
        let _applied = self.apply_ty_inner(ty, &mut visited);
        let mut comps: Vec<u32> = visited.cyclic_comps.into_iter().collect();
        let mut tys: Vec<u32> = visited.cyclic_tys.into_iter().collect();
        comps.sort_unstable();
        tys.sort_unstable();
        (comps, tys)
    }

    pub fn resolve_ty(&mut self, ty: &Ty) -> Ty {
        self.tys.resolve(ty)
    }

    pub fn resolve_comp_ty(&mut self, cty: &CompTy) -> CompTy {
        self.ctys.resolve(cty)
    }

    pub fn resolve_route(&mut self, route: &PayloadRoute) -> PayloadRoute {
        self.routes.resolve(route)
    }

    /// Canonicalize the head; variables nested in the result stay unresolved.
    pub fn resolve_row(&mut self, row: &Row) -> Row {
        self.rows.resolve(row)
    }

    /// Every `Extend` label, unsorted, and the terminal — `Some(v)` for an open
    /// row, `None` for one closed by `Empty`.  The loop needs no cycle guard:
    /// the occurs check rejects a cyclic row binding before it is installed.
    fn row_spine(&mut self, row: &Row) -> (Vec<String>, Option<RowVar>) {
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

    pub fn apply_ty(&mut self, ty: &Ty) -> Ty {
        let mut visited = Visited::default();
        self.apply_ty_inner(ty, &mut visited)
    }

    pub fn apply_comp_ty(&mut self, cty: &CompTy) -> CompTy {
        let mut visited = Visited::default();
        self.apply_comp_ty_inner(cty, &mut visited)
    }

    pub fn apply_row(&mut self, row: &Row) -> Row {
        let mut visited = Visited::default();
        self.apply_row_inner(row, &mut visited)
    }

    pub(super) fn apply_ty_inner(&mut self, ty: &Ty, visited: &mut Visited) -> Ty {
        // In CBPV every productive recursive value type closes through a
        // `Thunk` — μ values are finite, the recursion lives in the ν
        // computations — so a ty-back-edge onto `Thunk(Var(c))` with `c` on the
        // comp stack redirects to the comp anchor `c`, the canonical capture
        // point: `c` then lands in the scheme's `comp_ty_bindings` and gets a
        // fresh id per instantiation instead of being unrolled and shared.  A
        // cycle truly anchored at a ty-var reaches a `Variant`, not a `Thunk`,
        // and takes the plain fallback.
        let root = match ty {
            Ty::Var(TyVar(i)) => Some(self.tys.find(*i)),
            _ => None,
        };
        if let Some(r) = root {
            if visited.cyclic_tys.contains(&r) {
                return Ty::Var(TyVar(r));
            }
            if visited.tys.contains(&r) {
                // Match the raw binding, not `resolve_comp_ty` of it: the
                // anchor is `C`'s root, not whatever `C` resolves to now.
                if let Some(Ty::Thunk(b)) = self.tys.get(r)
                    && let CompTy::Var(CompTyVar(ci)) = *b
                {
                    let c_root = self.ctys.find(ci);
                    if visited.comps.contains(&c_root) {
                        visited.cyclic_comps.insert(c_root);
                        return Ty::Thunk(Box::new(CompTy::Var(CompTyVar(c_root))));
                    }
                }
                visited.cyclic_tys.insert(r);
                return Ty::Var(TyVar(r));
            }
        }
        let resolved = self.resolve_ty(ty);
        if matches!(&resolved, Ty::Var(_)) {
            return resolved;
        }
        if let Some(r) = root {
            visited.tys.insert(r);
        }
        let out = match resolved {
            Ty::List(a) => Ty::List(Box::new(self.apply_ty_inner(&a, visited))),
            Ty::Map(a) => Ty::Map(Box::new(self.apply_ty_inner(&a, visited))),
            Ty::Handle(a) => Ty::Handle(Box::new(self.apply_ty_inner(&a, visited))),
            Ty::Record(r) => Ty::Record(self.apply_row_inner(&r, visited)),
            Ty::Variant(r) => Ty::Variant(self.apply_row_inner(&r, visited)),
            Ty::Thunk(b) => Ty::Thunk(Box::new(self.apply_comp_ty_inner(&b, visited))),
            ground @ (Ty::Unit | Ty::Bytes | Ty::Bool | Ty::Int | Ty::Float | Ty::String) => ground,
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

    fn apply_comp_ty_inner(&mut self, cty: &CompTy, visited: &mut Visited) -> CompTy {
        // The anchor may sit on either side of the ty/comp boundary, but the
        // cycle traverses both, so every root currently expanding belongs to it
        // and must enter its bindings list to be given a fresh id later.
        let root = match cty {
            CompTy::Var(CompTyVar(i)) => Some(self.ctys.find(*i)),
            _ => None,
        };
        if let Some(r) = root {
            if visited.comps.contains(&r) {
                visited.cyclic_comps.insert(r);
                return CompTy::Var(CompTyVar(r));
            }
            if visited.cyclic_comps.contains(&r) {
                return CompTy::Var(CompTyVar(r));
            }
        }
        let resolved = self.resolve_comp_ty(cty);
        if matches!(&resolved, CompTy::Var(_)) {
            return resolved;
        }
        if let Some(r) = root {
            visited.comps.insert(r);
        }
        let out = match resolved {
            CompTy::Return(route, a) => CompTy::Return(
                self.resolve_route(&route),
                Box::new(self.apply_ty_inner(&a, visited)),
            ),
            CompTy::Fun(a, b) => CompTy::Fun(
                Box::new(self.apply_ty_inner(&a, visited)),
                Box::new(self.apply_comp_ty_inner(&b, visited)),
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

    fn apply_row_inner(&mut self, row: &Row, visited: &mut Visited) -> Row {
        match self.resolve_row(row) {
            Row::Empty => Row::Empty,
            Row::Var(v) => Row::Var(v),
            Row::Extend(l, ty, rest) => {
                let ty2 = self.apply_ty_inner(&ty, visited);
                let rest2 = self.apply_row_inner(&rest, visited);
                Row::Extend(l, Box::new(ty2), Box::new(rest2))
            }
        }
    }

    // Rows, unlike types, are inductive: `ρ = {l: τ}` with `ρ` reachable from
    // `τ` denotes an infinite record and is rejected as `RecursiveRow`.  A row
    // variable can hide in a field type as well as along the spine, so the check
    // descends through both, carrying a `Visited` because a field type may
    // legitimately be cyclic.

    fn row_occurs(&mut self, v: RowVar, row: &Row, depth: u32) -> Result<bool, TypeErrorKind> {
        let mut visited = Visited::default();
        self.row_occurs_inner(v, row, &mut visited, depth)
    }

    fn row_occurs_inner(
        &mut self,
        v: RowVar,
        row: &Row,
        visited: &mut Visited,
        depth: u32,
    ) -> Result<bool, TypeErrorKind> {
        match self.resolve_row(row) {
            Row::Empty => Ok(false),
            Row::Var(u) => Ok(u == v),
            Row::Extend(_, ty, rest) => Ok(self.ty_occurs_row(v, &ty, visited, deeper(depth)?)?
                || self.row_occurs_inner(v, &rest, visited, depth)?),
        }
    }

    fn ty_occurs_row(
        &mut self,
        v: RowVar,
        ty: &Ty,
        visited: &mut Visited,
        depth: u32,
    ) -> Result<bool, TypeErrorKind> {
        let root = match ty {
            Ty::Var(TyVar(i)) => Some(self.tys.find(*i)),
            _ => None,
        };
        if let Some(r) = root
            && !visited.tys.insert(r)
        {
            return Ok(false);
        }
        match self.resolve_ty(ty) {
            Ty::List(a) | Ty::Map(a) | Ty::Handle(a) => {
                self.ty_occurs_row(v, &a, visited, deeper(depth)?)
            }
            Ty::Record(r) | Ty::Variant(r) => self.row_occurs_inner(v, &r, visited, deeper(depth)?),
            Ty::Thunk(c) => self.comp_occurs_row(v, &c, visited, deeper(depth)?),
            // Enumerated rather than caught: a future row-embedding
            // constructor skipped here lets a cyclic row install undetected.
            Ty::Var(_) | Ty::Unit | Ty::Bytes | Ty::Bool | Ty::Int | Ty::Float | Ty::String => {
                Ok(false)
            }
        }
    }

    fn comp_occurs_row(
        &mut self,
        v: RowVar,
        cty: &CompTy,
        visited: &mut Visited,
        depth: u32,
    ) -> Result<bool, TypeErrorKind> {
        let root = match cty {
            CompTy::Var(CompTyVar(i)) => Some(self.ctys.find(*i)),
            _ => None,
        };
        if let Some(r) = root
            && !visited.comps.insert(r)
        {
            return Ok(false);
        }
        match self.resolve_comp_ty(cty) {
            CompTy::Var(_) => Ok(false),
            CompTy::Return(_, a) => self.ty_occurs_row(v, &a, visited, deeper(depth)?),
            CompTy::Fun(a, b) => Ok(self.ty_occurs_row(v, &a, visited, deeper(depth)?)?
                || self.comp_occurs_row(v, &b, visited, deeper(depth)?)?),
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
            Ty::Var(TyVar(i)) => TyKey::Var(self.tys.find(*i)),
        })
    }

    fn comp_key(&mut self, cty: &CompTy, depth: u32) -> Result<CompTyKey, TypeErrorKind> {
        Ok(match cty {
            CompTy::Return(route, t) => CompTyKey::Return(
                self.resolve_route(route),
                Box::new(self.ty_key(t, deeper(depth)?)?),
            ),
            CompTy::Fun(a, b) => CompTyKey::Fun(
                Box::new(self.ty_key(a, deeper(depth)?)?),
                Box::new(self.comp_key(b, deeper(depth)?)?),
            ),
            CompTy::Var(CompTyVar(i)) => CompTyKey::Var(self.ctys.find(*i)),
        })
    }

    fn row_key(&mut self, row: &Row, depth: u32) -> Result<RowKey, TypeErrorKind> {
        Ok(match row {
            Row::Empty => RowKey::Empty,
            Row::Var(RowVar(i)) => RowKey::Var(self.rows.find(*i)),
            Row::Extend(l, t, rest) => RowKey::Extend(
                l.clone(),
                Box::new(self.ty_key(t, deeper(depth)?)?),
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
    pub fn unify_ty(&mut self, a: &Ty, b: &Ty) -> Result<(), TypeErrorKind> {
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
            (Ty::Var(TyVar(ai)), Ty::Var(TyVar(bi))) => {
                let (ar, br) = (self.tys.find(*ai), self.tys.find(*bi));
                if guard_pair(&mut pairs.tys, ar, br) {
                    return Ok(());
                }
            }
            // One-sided, which is how a value-anchored cycle meets its
            // comp-anchored twin: the two never present as `Var`/`Var`.
            (Ty::Var(TyVar(vi)), other) | (other, Ty::Var(TyVar(vi))) => {
                let (root, key) = (self.tys.find(*vi), self.ty_key(other, depth)?);
                if guard_expansion(&mut pairs.ty_expansions, root, key) {
                    return Ok(());
                }
            }
            _ => {}
        }

        let a = self.resolve_ty(a);
        let b = self.resolve_ty(b);

        if let (Ty::Var(TyVar(ai)), Ty::Var(TyVar(bi))) = (&a, &b) {
            self.tys.unite(*ai, *bi);
            return Ok(());
        }
        if let Ty::Var(TyVar(vi)) = &a {
            // No occurs check: a cyclic binding is sound under the cycle-aware
            // traversals above.
            let r = self.tys.find(*vi);
            self.tys.bind(r, b);
            return Ok(());
        }
        if let Ty::Var(TyVar(vi)) = &b {
            let r = self.tys.find(*vi);
            self.tys.bind(r, a);
            return Ok(());
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
            // The one coercion: a record stands in for a homogeneous map.
            (Ty::Map(elem), Ty::Record(row)) | (Ty::Record(row), Ty::Map(elem)) => {
                self.unify_map_record(&elem, &row, pairs, depth)
            }
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
                expected: a,
                actual: b,
            }),
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
    pub fn unify_row(&mut self, a: &Row, b: &Row) -> Result<(), TypeErrorKind> {
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
        &mut self,
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
        let (a_labels, _) = self.row_spine(a);
        let known = if a_labels.contains(&label) {
            self.row_spine(b).0
        } else {
            a_labels
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
        let mut a = self.resolve_row(a);
        let mut b = self.resolve_row(b);
        loop {
            if let (Row::Var(RowVar(ai)), Row::Var(RowVar(bi))) = (&a, &b) {
                self.rows.unite(*ai, *bi);
                return Ok(());
            }
            if let Row::Var(RowVar(vi)) = &a {
                let vi = *vi;
                if self.row_occurs(RowVar(vi), &b, depth)? {
                    return Err(TypeErrorKind::RecursiveRow);
                }
                let r = self.rows.find(vi);
                self.rows.bind(r, b);
                return Ok(());
            }
            if let Row::Var(RowVar(vi)) = &b {
                let vi = *vi;
                if self.row_occurs(RowVar(vi), &a, depth)? {
                    return Err(TypeErrorKind::RecursiveRow);
                }
                let r = self.rows.find(vi);
                self.rows.bind(r, a);
                return Ok(());
            }

            match (a, b) {
                (Row::Empty, Row::Empty) => return Ok(()),
                (Row::Empty, Row::Extend(l, _, _)) => {
                    // The alternatives are named by whoever still holds the
                    // original rows; the rewrite below has consumed them here.
                    return Err(TypeErrorKind::RowExtraField {
                        label: l,
                        known: Vec::new(),
                    });
                }
                (Row::Extend(l, _, _), Row::Empty) => {
                    return Err(TypeErrorKind::RowMissingField { label: l });
                }
                (Row::Extend(l1, t1, r1), Row::Extend(l2, t2, r2)) => {
                    if l1 == l2 {
                        let (t1, t2) = (*t1, *t2);
                        self.unify_ty_inner(&t1, &t2, pairs, deeper(depth)?)?;
                        // In place, not a deeper frame: a wide row is O(1) stack.
                        a = self.resolve_row(&r1);
                        b = self.resolve_row(&r2);
                        continue;
                    }
                    // Tag and bare labels are disjoint alphabets: a row mixing
                    // them typechecks against neither pure form.
                    if is_tag_label(&l1) != is_tag_label(&l2) {
                        return Err(TypeErrorKind::TyMismatch {
                            expected: Ty::Record(Row::Extend(l1, t1.clone(), Box::new(Row::Empty))),
                            actual: Ty::Record(Row::Extend(l2, t2.clone(), Box::new(Row::Empty))),
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
                    if let (Some(t1), Some(t2)) = (left_tail, right_tail)
                        && t1 == t2
                    {
                        left.sort();
                        right.sort();
                        if left != right {
                            return Err(TypeErrorKind::RecursiveRow);
                        }
                    }
                    let rho = self.fresh_row_var();
                    let new_r1 = Row::Extend(l2, t2.clone(), Box::new(Row::Var(rho)));
                    let new_r2 = Row::Extend(l1, t1.clone(), Box::new(Row::Var(rho)));
                    self.unify_row_inner(&r1, &new_r1, pairs, depth)?;
                    return self.unify_row_inner(&r2, &new_r2, pairs, depth);
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
    pub fn unify_comp_ty(&mut self, a: &CompTy, b: &CompTy) -> Result<(), TypeErrorKind> {
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
            (CompTy::Var(CompTyVar(ai)), CompTy::Var(CompTyVar(bi))) => {
                let (ar, br) = (self.ctys.find(*ai), self.ctys.find(*bi));
                if guard_pair(&mut pairs.comps, ar, br) {
                    return Ok(());
                }
            }
            // One-sided: the comp half of the anchoring mismatch, `F T ~= C`.
            (CompTy::Var(CompTyVar(vi)), other) | (other, CompTy::Var(CompTyVar(vi))) => {
                let (root, key) = (self.ctys.find(*vi), self.comp_key(other, depth)?);
                if guard_expansion(&mut pairs.comp_expansions, root, key) {
                    return Ok(());
                }
            }
            _ => {}
        }

        let a = self.resolve_comp_ty(a);
        let b = self.resolve_comp_ty(b);

        if let (CompTy::Var(CompTyVar(ai)), CompTy::Var(CompTyVar(bi))) = (&a, &b) {
            self.ctys.unite(*ai, *bi);
            return Ok(());
        }
        if let CompTy::Var(CompTyVar(vi)) = &a {
            // No occurs check, for the same reason as `unify_ty_inner`.
            let r = self.ctys.find(*vi);
            self.ctys.bind(r, b);
            return Ok(());
        }
        if let CompTy::Var(CompTyVar(vi)) = &b {
            let r = self.ctys.find(*vi);
            self.ctys.bind(r, a);
            return Ok(());
        }
        let depth = deeper(depth)?;
        match (a, b) {
            (CompTy::Return(ra, ta), CompTy::Return(rb, tb)) => {
                let mut diffs: Vec<CompDiff> = Vec::new();
                if self.unify_route(&ra, &rb).is_err() {
                    diffs.push(CompDiff::Route {
                        expected: self.resolve_route(&ra),
                        actual: self.resolve_route(&rb),
                    });
                }
                // A return-type disagreement folds into the rich `Return` diff,
                // but a spent depth budget is exhaustion rather than
                // disagreement, so it propagates verbatim.
                match self.unify_ty_inner(&ta, &tb, pairs, depth) {
                    Ok(()) => {}
                    Err(TypeErrorKind::TypeTooDeep) => return Err(TypeErrorKind::TypeTooDeep),
                    Err(_) => diffs.push(CompDiff::ReturnType {
                        expected: self.apply_ty(&ta),
                        actual: self.apply_ty(&tb),
                    }),
                }
                if diffs.is_empty() {
                    Ok(())
                } else {
                    Err(TypeErrorKind::CompTyMismatch {
                        expected: self.apply_return(ra, &ta),
                        actual: self.apply_return(rb, &tb),
                        diffs,
                    })
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
                    diffs: Vec::new(),
                })
            }
        }
    }

    /// Rebuild a `CompTy::Return` post-substitution, for mismatch diagnostics.
    fn apply_return(&mut self, route: PayloadRoute, ty: &Ty) -> CompTy {
        CompTy::Return(self.resolve_route(&route), Box::new(self.apply_ty(ty)))
    }

    /// Unify two payload routes by *equality*: two variables unite, a
    /// variable and a ground route bind, two ground routes must agree.  A
    /// route names where a value boundary reads a computation's payload,
    /// never what it writes, so `Value` and `Bytes` never unify silently.
    ///
    /// No caller passes a bare `Bytes` in here: WF-2 admits exactly one
    /// byte-routed computation, so a decision that lands on the byte side
    /// unifies with [`CompTy::bytes`] whole, and the `Unit` pairing travels
    /// with the route instead of resting on the caller's memory.  Ground
    /// `Bytes` reaches this function only riding a type (`unify_comp_ty`)
    /// or already resolved on both sides.
    ///
    /// # Errors
    /// [`RouteMismatch`] for distinct ground routes, which each caller maps
    /// onto its own diagnostic.
    pub fn unify_route(&mut self, a: &PayloadRoute, b: &PayloadRoute) -> Result<(), RouteMismatch> {
        let a = self.resolve_route(a);
        let b = self.resolve_route(b);
        match (a, b) {
            (PayloadRoute::Var(PayloadVar(va)), PayloadRoute::Var(PayloadVar(vb))) => {
                self.routes.unite(va, vb);
                Ok(())
            }
            (PayloadRoute::Var(PayloadVar(v)), g) | (g, PayloadRoute::Var(PayloadVar(v))) => {
                let r = self.routes.find(v);
                self.routes.bind(r, g);
                Ok(())
            }
            (PayloadRoute::Value, PayloadRoute::Value)
            | (PayloadRoute::Bytes, PayloadRoute::Bytes) => Ok(()),
            (left, right) => Err(RouteMismatch { left, right }),
        }
    }

    /// The spine is width; only a field *type* against the element goes deeper.
    fn unify_map_record(
        &mut self,
        elem: &Ty,
        row: &Row,
        pairs: &mut Pairs,
        depth: u32,
    ) -> Result<(), TypeErrorKind> {
        let row = self.resolve_row(row);
        match row {
            Row::Empty => Ok(()),
            Row::Var(RowVar(vi)) => {
                let r = self.rows.find(vi);
                self.rows.bind(r, Row::Empty);
                Ok(())
            }
            Row::Extend(_, ty, rest) => {
                let ty = *ty;
                self.unify_ty_inner(&ty, elem, pairs, deeper(depth)?)?;
                self.unify_map_record(elem, &rest, pairs, depth)
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
fn ordered_pair(a: u32, b: u32) -> (u32, u32) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Symmetric obligation: two var roots.  `true` when in progress or coincident.
fn guard_pair(seen: &mut HashSet<(u32, u32)>, a: u32, b: u32) -> bool {
    a == b || !seen.insert(ordered_pair(a, b))
}

/// One-sided obligation: a var root against a key.  `true` when in progress.
fn guard_expansion<K: Eq + std::hash::Hash>(
    seen: &mut HashSet<(u32, K)>,
    root: u32,
    key: K,
) -> bool {
    !seen.insert((root, key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{HEAD_FIELD, TAIL_FIELD, done_tag, more_tag};

    /// ``Variant{`more: {head: String, tail: Thunk(tail)}, `done: Unit}``.
    fn step(tail: CompTy) -> Ty {
        let payload = Ty::Record(Row::Extend(
            HEAD_FIELD.into(),
            Box::new(Ty::String),
            Box::new(Row::Extend(
                TAIL_FIELD.into(),
                Box::new(Ty::Thunk(Box::new(tail))),
                Box::new(Row::Empty),
            )),
        ));
        Ty::Variant(Row::Extend(
            more_tag(),
            Box::new(payload),
            Box::new(Row::Extend(
                done_tag(),
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
        let t_root = u.ty_root(t.0);
        let t_body = step(CompTy::pure(Ty::Var(t)));
        u.bind_ty_root(t_root, t_body);

        let CompTy::Var(CompTyVar(c_root)) = u.fresh_comp_ty() else {
            unreachable!("fresh_comp_ty yields a Var")
        };
        let step_c = step(CompTy::Var(CompTyVar(c_root)));
        u.bind_comp_root(c_root, CompTy::pure(step_c.clone()));

        u.unify_ty(&Ty::Var(t), &step_c)
            .expect("equi-recursive stream types unify regardless of anchor");
    }

    /// The one-sided obligation guard keys a variable root against a
    /// *fingerprint* of the other side, so two obligations that differ only in
    /// payload route must land on two different keys.  Before this change the
    /// fingerprint carried `input` and `output` and not the route; deleting
    /// those two without installing the route in their place compiles
    /// perfectly and silently coarsens what a `CompTy::Var` obligation can
    /// tell apart, which is why this is a test and not a reading.
    #[test]
    fn a_one_sided_obligation_distinguishes_the_payload_route() {
        let mut u = Unifier::new();
        let CompTy::Var(CompTyVar(root)) = u.fresh_comp_ty() else {
            unreachable!("fresh_comp_ty yields a Var")
        };
        let captured = u
            .comp_key(&CompTy::Return(PayloadRoute::Bytes, Box::new(Ty::Unit)), 0)
            .expect("a ground key");
        let returned = u
            .comp_key(&CompTy::Return(PayloadRoute::Value, Box::new(Ty::Unit)), 0)
            .expect("a ground key");
        assert!(
            captured != returned,
            "a captured-from-stdout command and a Unit-returning one are not the same obligation"
        );

        let mut seen = HashSet::new();
        assert!(
            !guard_expansion(&mut seen, root, captured),
            "the first obligation is new"
        );
        assert!(
            !guard_expansion(&mut seen, root, returned),
            "an obligation differing only in route must not be discharged as one already in progress"
        );
    }

    /// A `List(List(… Int …))` spine: no variable, so nothing to memoize.
    fn deep_list(n: u32) -> Ty {
        let mut ty = Ty::Int;
        for _ in 0..n {
            ty = Ty::List(Box::new(ty));
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
                "x".into(),
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
            Row::Extend((*l).into(), Box::new(t.clone()), Box::new(rest))
        })
    }

    fn open_row(fields: &[(&str, Ty)], tail: RowVar) -> Row {
        fields.iter().rev().fold(Row::Var(tail), |rest, (l, t)| {
            Row::Extend((*l).into(), Box::new(t.clone()), Box::new(rest))
        })
    }

    fn resolved_fields(u: &mut Unifier, row: &Row) -> std::collections::HashMap<String, Ty> {
        let mut out = std::collections::HashMap::new();
        let mut cur = u.apply_row(row);
        loop {
            match cur {
                Row::Extend(l, t, rest) => {
                    out.insert(l, *t);
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
        let fields = resolved_fields(&mut u, &Row::Var(rho));
        assert_eq!(
            fields.get("b"),
            Some(&Ty::String),
            "tail absorbed the `b` field"
        );
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
}
