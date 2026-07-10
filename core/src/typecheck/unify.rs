//! Union-find unifier over four variable kinds: type, computation type, mode, row.
//!
//! A single `Store<T>` handles the union-find plumbing for any payload type T.
//! Kind-specific unify methods on `Unifier` encode structural rules (including
//! the one coercion preserved by design: Record↔Map).
//! Pipeline modes are *not* coerced: [`Unifier::unify_mode`] is
//! equality-strict so a value edge cannot silently meet a byte edge
//! (`docs/SPEC.md` §4.2.1, §20.4).
//!
//! Both value types and computation types are *equi-recursive*: a binding such
//! as `comp_ty_slots[N] = Bound(Fun(Int, Var(N)))` or `ty_slots[N] = Bound(
//! Variant {`more {head: T, tail: Thunk(Return(Var(N)))}, `done | ρ})` is
//! allowed and represents a self-referential type.  Every traversal that
//! descends through `Thunk` / `Fun` / `Return` / `List` / `Variant` / … carries
//! a `Visited` of {ty, comp}-var roots in the current expansion so a cycle
//! returns a back-edge instead of recursing forever.  Unification carries a
//! co-inductive `Pairs` of equality obligations already in progress —
//! symmetric {ty, comp}-var *root pairs*, plus *one-sided* obligations
//! pairing a var root with a finite structural key of the other side — so
//! unifying two cyclic types reaches a fixed-point instead of looping, even
//! when the same equi-recursive type is anchored at a ty-var on one side and
//! a comp-var on the other.  No occurs check is needed in either kind.

use super::scheme::{CompDiff, TypeErrorKind};
use super::ty::{CompTy, CompTyVar, ModeVar, PipeMode, PipeSpec, Row, RowVar, Ty, TyVar};
use crate::syntax::tag::is_tag_label;
use std::collections::HashSet;

// ─────────────────────────────────────────────────────────────────────────────
// Cycle-tracking bundles
// ─────────────────────────────────────────────────────────────────────────────

/// Cycle-tracking state threaded through `apply_*` and `free_*`.
///
/// `tys` and `comps` hold roots currently on the active recursion path —
/// `apply_*` treats them as a stack (push on entry, pop on exit unless
/// the root proved cyclic), while `free_*` treats them as a set (insert
/// once, never remove) since free-var collection is idempotent.
///
/// `cyclic_tys` and `cyclic_comps` are populated by `apply_*` when a
/// back-edge is discovered: the root then becomes "permanently visited"
/// for the rest of the call so sibling references to a shared cyclic
/// subtree continue to yield `Var(root)` back-edges instead of unrolling
/// the binding at every position.  Non-cyclic roots leave these sets
/// alone, so a sibling reference to a `τ → Int` root re-resolves to
/// `Int` instead of being mistaken for a cycle.
#[derive(Default)]
pub(super) struct Visited {
    pub tys: HashSet<u32>,
    pub comps: HashSet<u32>,
    cyclic_tys: HashSet<u32>,
    cyclic_comps: HashSet<u32>,
}

/// Co-inductive guard for unification: equality obligations already in
/// progress.  Re-entering the same obligation is an immediate success —
/// that is what makes unifying two cyclic types terminate at a fixed-point.
///
/// Two anchorings of the *same* equi-recursive type need not present as a
/// `Var`/`Var` pair: a value-var-anchored stream (`T = Step(F T)`) meets a
/// comp-var-anchored one (`C = F Step(C)`) as `Var`-vs-concrete-structure
/// (`T ~= Step(C)`, `F T ~= C`).  So alongside the symmetric root pairs we
/// memoize *one-sided* obligations: a variable root against a finite
/// structural key of the other side ([`TyKey`] / [`CompTyKey`], which
/// canonicalize nested vars through `find` but never expand bindings).
#[derive(Default)]
struct Pairs {
    tys: HashSet<(u32, u32)>,
    comps: HashSet<(u32, u32)>,
    ty_expansions: HashSet<(u32, TyKey)>,
    comp_expansions: HashSet<(u32, CompTyKey)>,
}

/// Defensive ceiling on *true nesting depth* — the number of times a
/// structural traversal descends into a genuinely deeper subterm
/// (a list/map/handle element, a thunk body, a function argument or
/// result, a record/variant field type).  Iterating across the sibling
/// fields of one row spine is width, not depth, so it never charges
/// against this budget: a wide-but-shallow record unifies freely.
///
/// The co-inductive `Pairs` guard terminates every *cyclic* obligation;
/// this bound is the secondary, structural stop for a variable-free type
/// nested past any plausible source program, turning a would-be stack
/// overflow into a graceful [`TypeErrorKind::TypeTooDeep`].  The budget is
/// shared by every structural-recursion path reachable during unification
/// — the unify arms, the key fingerprints of the one-sided obligations
/// ([`Unifier::ty_key`] / [`Unifier::comp_key`] / [`Unifier::row_key`]),
/// and the row occurs check ([`Unifier::row_occurs`]) — so no descent can
/// escape the bound by crossing from one cluster into another.  No
/// reproducing program is known; the limit sits far above realistic
/// nesting (programs nest a handful of levels) yet comfortably under the
/// ~900-frame depth at which the descent exhausts even a conservatively
/// small (~2 MiB) runtime stack in a debug build.
const MAX_UNIFY_DEPTH: u32 = 512;

/// Charge one level of genuine descent against [`MAX_UNIFY_DEPTH`],
/// yielding the deeper count or a graceful [`TypeErrorKind::TypeTooDeep`]
/// when the budget is spent.  Called only when stepping into a strictly
/// deeper subterm — never when walking sideways along a row spine.
fn deeper(depth: u32) -> Result<u32, TypeErrorKind> {
    if depth >= MAX_UNIFY_DEPTH {
        return Err(TypeErrorKind::TypeTooDeep);
    }
    Ok(depth + 1)
}

/// Finite structural fingerprint of a value type, used as the non-variable
/// half of a one-sided unification obligation.  Variables canonicalize to
/// their union-find root via `find`; bindings are *not* expanded, so the
/// key stays finite even when the root is bound to a cyclic structure.
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

/// Finite structural fingerprint of a computation type.  See [`TyKey`].
#[derive(Clone, PartialEq, Eq, Hash)]
enum CompTyKey {
    Return(PipeMode, PipeMode, Box<TyKey>),
    Fun(Box<TyKey>, Box<Self>),
    Var(u32),
}

/// Finite structural fingerprint of a row spine.  See [`TyKey`].
#[derive(Clone, PartialEq, Eq, Hash)]
enum RowKey {
    Empty,
    Var(u32),
    Extend(String, Box<TyKey>, Box<Self>),
}

// ─────────────────────────────────────────────────────────────────────────────
// Generic union-find store
// ─────────────────────────────────────────────────────────────────────────────

enum Slot<T> {
    Free,
    Bound(T),
    Parent(u32),
}

struct Store<T> {
    slots: Vec<Slot<T>>,
    next: u32,
}

/// A sort whose terms include variables drawn from a union-find store:
/// `as_var` projects out the variable id when a term is a bare variable,
/// and `from_root` rebuilds a variable term at a canonical root id.
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

impl Unifiable for PipeMode {
    fn as_var(&self) -> Option<u32> {
        match self {
            Self::Var(ModeVar(i)) => Some(*i),
            _ => None,
        }
    }
    fn from_root(root: u32) -> Self {
        Self::Var(ModeVar(root))
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
        // Out-of-range IDs belong to a foreign unifier (e.g. cached prelude
        // schemes loaded into a fresh InferCtx).  Treat them as free.
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

    /// Follow a variable to its root and clone the bound value, if any.
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

    /// Auto-expand for out-of-range IDs — cached prelude vars sometimes
    /// arrive at a fresh unifier above its `next`.  Newly inserted slots
    /// are Free.
    fn ensure(&mut self, i: u32) {
        let needed = (i as usize) + 1;
        if needed > self.slots.len() {
            self.slots.resize_with(needed, || Slot::Free);
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

    /// Var/var union-find prelude shared by every kind: same id is a noop;
    /// otherwise union the roots.
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
    /// Follow a variable chain to canonical form: a bound variable expands
    /// to its (recursively resolved) binding; an unbound variable becomes a
    /// variable at its union-find root; a non-variable term is returned as
    /// is.  A `Var → Var` chain only arises from `unite`, which writes
    /// `Slot::Parent`, so `find` collapses it before any binding is reached.
    /// The walk stops at the first non-variable head, leaving structurally
    /// nested variables (the back-edges of a cyclic binding) untouched.
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

// ─────────────────────────────────────────────────────────────────────────────
// Unifier
// ─────────────────────────────────────────────────────────────────────────────

pub struct Unifier {
    tys: Store<Ty>,
    ctys: Store<CompTy>,
    modes: Store<PipeMode>,
    rows: Store<Row>,
}

impl Unifier {
    pub fn new() -> Self {
        Self {
            tys: Store::new(),
            ctys: Store::new(),
            modes: Store::new(),
            rows: Store::new(),
        }
    }

    pub fn fresh_tyvar(&mut self) -> TyVar {
        TyVar(self.tys.fresh())
    }
    pub fn fresh_ty(&mut self) -> Ty {
        Ty::Var(self.fresh_tyvar())
    }

    pub fn fresh_modevar(&mut self) -> ModeVar {
        ModeVar(self.modes.fresh())
    }
    pub fn fresh_mode(&mut self) -> PipeMode {
        PipeMode::Var(self.fresh_modevar())
    }
    /// A `PipeSpec` whose input and output modes are both fresh variables —
    /// the unconstrained `F[μ, ν]` shape used wherever a head's pipeline
    /// modes are unknown.
    pub fn fresh_spec(&mut self) -> PipeSpec {
        PipeSpec {
            input: self.fresh_mode(),
            output: self.fresh_mode(),
        }
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

    /// Canonical comp-var root id under union-find — used by cycle-aware
    /// traversals in sibling modules (env.rs, generalize.rs).
    pub fn comp_root(&mut self, i: u32) -> u32 {
        self.ctys.find(i)
    }

    /// Canonical ty-var root id under union-find — used by cycle-aware
    /// traversals in sibling modules (env.rs, generalize.rs).
    pub fn ty_root(&mut self, i: u32) -> u32 {
        self.tys.find(i)
    }

    /// Allocate a fresh comp-var slot and return its root id.  Used by
    /// scheme instantiation to mint independent slots for each cyclic
    /// comp-var in a polymorphic recursive scheme.
    pub fn fresh_comp_root(&mut self) -> u32 {
        self.ctys.fresh()
    }

    /// Allocate a fresh ty-var slot and return its root id.  Mirror of
    /// `fresh_comp_root` for cyclic ty bindings.
    pub fn fresh_ty_root(&mut self) -> u32 {
        self.tys.fresh()
    }

    /// Bind a freshly-minted comp-var root to a `CompTy` value.  Pairs
    /// with `fresh_comp_root` for instantiation: the binding is the
    /// scheme's snapshot rewritten through the substitution map.
    pub fn bind_comp_root(&mut self, root: u32, value: CompTy) {
        self.ctys.bind(root, value);
    }

    /// Bind a freshly-minted ty-var root to a `Ty` value.  Mirror of
    /// `bind_comp_root` for cyclic ty bindings.
    pub fn bind_ty_root(&mut self, root: u32, value: Ty) {
        self.tys.bind(root, value);
    }

    /// If the comp-var root has a non-Var binding, return its current
    /// (resolved, applied) value; otherwise `None`.  Used by
    /// `generalize` to snapshot cyclic comp-var bindings for storage
    /// in the scheme.
    ///
    /// Quote *from the root* (`apply` of `Var(root)`), not from the
    /// stored body — applying the raw binding starts the walk one
    /// level below the anchor and would unroll the cycle once before
    /// the back-edge fires, leaving the snapshot off by a level and
    /// leaking the original union-find slot at the leaked level.
    pub fn resolved_comp_root_binding(&mut self, root: u32) -> Option<CompTy> {
        match self.ctys.get(root) {
            Some(CompTy::Var(_)) | None => None,
            Some(_) => Some(self.apply_comp_ty(&CompTy::Var(CompTyVar(root)))),
        }
    }

    /// Mirror of `resolved_comp_root_binding` for cyclic ty bindings.
    /// Same anchor-quoting requirement applies.
    pub fn resolved_ty_root_binding(&mut self, root: u32) -> Option<Ty> {
        match self.tys.get(root) {
            Some(Ty::Var(_)) | None => None,
            Some(_) => Some(self.apply_ty(&Ty::Var(TyVar(root)))),
        }
    }

    /// Collect comp-var roots that appear in any cycle reachable from `ty`.
    /// Walks via `apply_ty_inner` so that mid-cycle roots — pushed onto
    /// the recursion stack but not yet returned as back-edges in the
    /// applied output — are still captured (`apply_*_inner` tags them
    /// via `mark_stack_cyclic` on cycle detection).
    pub fn cyclic_comp_roots_in_ty(&mut self, ty: &Ty) -> Vec<u32> {
        let mut visited = Visited::default();
        let _applied = self.apply_ty_inner(ty, &mut visited);
        let mut out: Vec<u32> = visited.cyclic_comps.into_iter().collect();
        out.sort_unstable();
        out
    }

    /// Mirror of `cyclic_comp_roots_in_ty` for cyclic value-type bindings.
    pub fn cyclic_ty_roots_in_ty(&mut self, ty: &Ty) -> Vec<u32> {
        let mut visited = Visited::default();
        let _applied = self.apply_ty_inner(ty, &mut visited);
        let mut out: Vec<u32> = visited.cyclic_tys.into_iter().collect();
        out.sort_unstable();
        out
    }

    // ── Resolve: follow variable chains to canonical form ────────────────────

    pub fn resolve_ty(&mut self, ty: &Ty) -> Ty {
        self.tys.resolve(ty)
    }

    pub fn resolve_comp_ty(&mut self, cty: &CompTy) -> CompTy {
        self.ctys.resolve(cty)
    }

    pub fn resolve_mode(&mut self, mode: &PipeMode) -> PipeMode {
        self.modes.resolve(mode)
    }

    /// Follow row variable bindings to canonical form.
    /// The returned row may still contain nested unresolved variables.
    pub fn resolve_row(&mut self, row: &Row) -> Row {
        self.rows.resolve(row)
    }

    /// Walk a row to its terminal tail, collecting the labels of every
    /// `Extend` along the spine.  Returns the label multiset (unsorted) and
    /// the terminal: `Some(root)` for an open row whose tail is a variable,
    /// `None` for one closed by `Empty`.  Each tail is resolved before the
    /// step, so binding cycles cannot occur — the occurs check rejects them
    /// before they are installed.
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

    // ── Apply: recursively substitute all variables ──────────────────────────

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
        // Cycle guard with Thunk-descent at ty-back-edge.  In CBPV ral
        // every productive recursive value type closes through a `Thunk`
        // wrapper (μ values are finite; ν computations are where the
        // recursion lives).  When a ty-back-edge fires and the binding is
        // `Thunk(Var(c))` with `c` already on the comp stack, redirect
        // the back-edge to the comp anchor `c` — that's the canonical
        // place to capture the cycle for the snapshot, so `c` ends up in
        // `comp_ty_bindings` and gets a fresh id per instantiation
        // instead of being unrolled and shared.
        //
        // The plain `Ty::Var(r)` fallback handles value cycles anchored at
        // a ty-var (e.g. a stream combinator written to take a `Stream`
        // *value* rather than a `Thunk`): the binding reached at the
        // back-edge is a `Variant`, not `Thunk(Var(c))`, so there is no
        // comp anchor to redirect to and the ty-var is the canonical cycle
        // point.  Unification handles the same shape co-inductively via the
        // one-sided obligations in `Pairs`.
        let root = match ty {
            Ty::Var(TyVar(i)) => Some(self.tys.find(*i)),
            _ => None,
        };
        if let Some(r) = root {
            if visited.cyclic_tys.contains(&r) {
                return Ty::Var(TyVar(r));
            }
            if visited.tys.contains(&r) {
                // Match the raw bound CompTy (no resolve_comp_ty) so that
                // a stored `Thunk(Var(C))` is recognised even when `C` is
                // itself bound — the canonical anchor is `C`'s root, not
                // the structure `C` happens to resolve to right now.
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
            // `Ty::Var` was stripped above: `resolve_ty` returns a `Var`
            // only when unbound, and that case early-returned before this
            // match.  Enumerating every other constructor means a new one
            // fails the build here instead of falling through unsubstituted.
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
        // Hybrid cycle guard with whole-stack cyclic marking on back-edge.
        // The cycle's anchor may be either side of the ty/comp boundary,
        // but in CBPV ral the cycle physically traverses BOTH — every
        // root currently expanding is part of the cycle and must enter
        // its respective bindings list so instantiation gets fresh ids.
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
            CompTy::Return(spec, a) => CompTy::Return(
                PipeSpec {
                    input: self.resolve_mode(&spec.input),
                    output: self.resolve_mode(&spec.output),
                },
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

    // ── Row occurs check ─────────────────────────────────────────────────────
    //
    // Rows are inductive: a row variable may appear in the spine *or* nested
    // inside a field type (e.g. `Record(ρ)` as the payload of an `Extend`), so
    // the check descends through field types as well as the spine.  A row
    // binding `ρ = {l: τ}` with `ρ` reachable from `τ` would denote an infinite
    // record and is rejected as [`TypeErrorKind::RecursiveRow`] rather than
    // installed.  Value and computation types are themselves equi-recursive,
    // so the descent through field types carries a [`Visited`] of {ty, comp}-var
    // roots to fold a legitimate cyclic field type into a back-edge instead of
    // looping.

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
            // Each field type is a strictly deeper subterm (`deeper`); the
            // walk to the next spine entry is width, so `rest` reuses
            // `depth` untouched.
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
            // Ground leaves carry no row variable.  Enumerated rather than a
            // catch-all so a future constructor that *does* embed a row (or a
            // type that could) fails the build here and is consciously routed,
            // not silently skipped — an under-checked occurs check would let a
            // cyclic row install undetected.
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

    // ── Structural keys for one-sided obligations ───────────────────────────
    //
    // A key fingerprints the *given term* (a finite syntactic type), not its
    // equi-recursive expansion: a `Var` collapses to its union-find root and
    // the walk stops there, so the key is finite even when that root is bound
    // to a cyclic structure.  Equal keys mean the same obligation against the
    // same anchor, which is the cyclic fixed point the guard discharges.
    //
    // The fingerprint is built by structural recursion, so it shares the
    // [`MAX_UNIFY_DEPTH`] budget threaded from the calling unify arm
    // (`deeper` on each strictly deeper subterm, width along a row spine left
    // uncharged) — a variable-free type nested past the ceiling surfaces
    // [`TypeErrorKind::TypeTooDeep`] here rather than overflowing the stack.

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
            CompTy::Return(spec, t) => CompTyKey::Return(
                self.resolve_mode(&spec.input),
                self.resolve_mode(&spec.output),
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
            // The field type is a deeper subterm; the spine tail (`rest`) is
            // width, so it reuses `depth`.
            Row::Extend(l, t, rest) => RowKey::Extend(
                l.clone(),
                Box::new(self.ty_key(t, deeper(depth)?)?),
                Box::new(self.row_key(rest, depth)?),
            ),
        })
    }

    // ── Unification ──────────────────────────────────────────────────────────

    /// Unify value types `a` and `b`, binding variables in place.
    ///
    /// # Errors
    /// Returns `Err` if the two types have mismatched structure
    /// ([`TypeErrorKind::TyMismatch`]), if an embedded row unification forms
    /// a recursive row ([`TypeErrorKind::RecursiveRow`]), or if the terms
    /// nest past the depth budget ([`TypeErrorKind::TypeTooDeep`]).
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
        // Co-inductive guard.  Re-entering the same equality obligation means
        // the recursion has reached its cyclic fixed point — discharge it.
        match (a, b) {
            // Symmetric: two ty-var roots already in progress.
            (Ty::Var(TyVar(ai)), Ty::Var(TyVar(bi))) => {
                let (ar, br) = (self.tys.find(*ai), self.tys.find(*bi));
                if guard_pair(&mut pairs.tys, ar, br) {
                    return Ok(());
                }
            }
            // One-sided: a ty-var root against a structural key of the other
            // side.  This catches a value-var-anchored cycle meeting the same
            // type anchored at a comp-var, where the two never present as a
            // `Var`/`Var` pair.
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
            // No occurs check: value types are equi-recursive.  Cyclic
            // bindings in the union-find are sound under the cycle-aware
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
                self.unify_row_inner(&r1, &r2, pairs, depth)
            }
            (Ty::Thunk(a1), Ty::Thunk(b1)) => self.unify_comp_ty_inner(&a1, &b1, pairs, depth),
            // Record ↔ Map coercion: a record can be used where a homogeneous map
            // is expected if all its field types unify to the map's element type.
            (Ty::Map(elem), Ty::Record(row)) | (Ty::Record(row), Ty::Map(elem)) => {
                self.unify_map_record(&elem, &row, pairs, depth)
            }
            (a, b) => Err(TypeErrorKind::TyMismatch {
                expected: a,
                actual: b,
            }),
        }
    }

    /// Row unification using the Rémy rewrite rule.
    ///
    /// # Errors
    /// Returns `Err` if a closed row is missing or carries an extra label the
    /// other requires ([`TypeErrorKind::RowMissingField`] /
    /// [`RowExtraField`](TypeErrorKind::RowExtraField)), if a shared label's
    /// field types clash or a tag/bare label alphabet is mixed
    /// ([`TypeErrorKind::TyMismatch`]), if the rewrite forms a recursive row
    /// ([`TypeErrorKind::RecursiveRow`]), or if the row nests past the depth
    /// budget ([`TypeErrorKind::TypeTooDeep`]).
    pub fn unify_row(&mut self, a: &Row, b: &Row) -> Result<(), TypeErrorKind> {
        let mut pairs = Pairs::default();
        self.unify_row_inner(a, b, &mut pairs, 0)
    }

    /// `depth` is the genuine nesting depth at which this row sits — set by
    /// the record/variant arm that descended into it.  Walking the row's own
    /// spine is *width*, not depth: the matched-label common case iterates
    /// down the spine in this `loop` (so a wide record never grows the call
    /// stack one frame per field), and the Rémy re-entries below thread
    /// `depth` unchanged.  Only stepping into a field *type* (`unify_ty_inner`
    /// on `t1`/`t2`) charges a level via `deeper`.
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

            // The three Row::Var early-return blocks above strip the Var
            // case from both sides, so the match below ranges over {Empty,
            // Extend} × {Empty, Extend} — four combinations, all explicit.
            match (a, b) {
                (Row::Empty, Row::Empty) => return Ok(()),
                (Row::Empty, Row::Extend(l, _, _)) => {
                    return Err(TypeErrorKind::RowExtraField { label: l });
                }
                (Row::Extend(l, _, _), Row::Empty) => {
                    return Err(TypeErrorKind::RowMissingField { label: l });
                }
                (Row::Extend(l1, t1, r1), Row::Extend(l2, t2, r2)) => {
                    if l1 == l2 {
                        let (t1, t2) = (*t1, *t2);
                        self.unify_ty_inner(&t1, &t2, pairs, deeper(depth)?)?;
                        // Step down the shared spine in place — width, not a
                        // deeper frame — so a wide row stays O(1) in stack.
                        a = self.resolve_row(&r1);
                        b = self.resolve_row(&r2);
                        continue;
                    }
                    // Reject mixed-alphabet rows: tag labels (`` `l ``) and
                    // bare labels (`l`) are disjoint by design — a record literal
                    // with both shapes never typechecks against either pure
                    // form, and a variant row must be all-tag.
                    if is_tag_label(&l1) != is_tag_label(&l2) {
                        return Err(TypeErrorKind::TyMismatch {
                            expected: Ty::Record(Row::Extend(
                                l1.clone(),
                                t1.clone(),
                                Box::new(Row::Empty),
                            )),
                            actual: Ty::Record(Row::Extend(
                                l2.clone(),
                                t2.clone(),
                                Box::new(Row::Empty),
                            )),
                        });
                    }
                    // Scoped-labels side condition (Gaster–Jones, Leijen):
                    // when both rows terminate in the *same* tail variable but
                    // carry different label multisets, the Rémy rewrite would
                    // bind that tail to a row over a fresh tail and re-enter
                    // with the mismatch intact — recursing forever, minting a
                    // new tail each turn.  No finite or rational row satisfies
                    // the equation, so it is reported as
                    // [`TypeErrorKind::RecursiveRow`].  Resolving only the two
                    // immediate rests misses this when the disagreement sits
                    // deeper than the head (`{x,a|ρ} ≐ {y,b|ρ}`), so the whole
                    // spine of each side is walked.  A permutation — same tail,
                    // same multiset (`{a,b|ρ} ≐ {b,a|ρ}`) — does have a
                    // solution and must still take the rewrite below.
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
                    let new_r1 = Row::Extend(l2.clone(), t2.clone(), Box::new(Row::Var(rho)));
                    let new_r2 = Row::Extend(l1.clone(), t1.clone(), Box::new(Row::Var(rho)));
                    self.unify_row_inner(&r1, &new_r1, pairs, depth)?;
                    return self.unify_row_inner(&r2, &new_r2, pairs, depth);
                }
                _ => unreachable!("Row::Var pairs are handled by the early-return blocks above"),
            }
        }
    }

    /// Unify computation types `a` and `b`, binding variables in place.
    ///
    /// # Errors
    /// Returns `Err` if the two computation types have mismatched structure or
    /// their pipeline modes or return types disagree
    /// ([`TypeErrorKind::CompTyMismatch`]), or if a component value type nests
    /// past the depth budget ([`TypeErrorKind::TypeTooDeep`]).
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
        // Co-inductive guard.  Re-entering the same equality obligation means
        // the recursion has reached its cyclic fixed point — discharge it.
        match (a, b) {
            // Symmetric: two comp-var roots already in progress.
            (CompTy::Var(CompTyVar(ai)), CompTy::Var(CompTyVar(bi))) => {
                let (ar, br) = (self.ctys.find(*ai), self.ctys.find(*bi));
                if guard_pair(&mut pairs.comps, ar, br) {
                    return Ok(());
                }
            }
            // One-sided: a comp-var root against a structural key of the
            // other side — the comp-level half of the value-var/comp-var
            // anchoring mismatch (`F T ~= C`).
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
            // No occurs check: comp types are equi-recursive.  Cyclic
            // bindings in the union-find are sound under the cycle-aware
            // traversals above.
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
            (CompTy::Return(sa, ta), CompTy::Return(sb, tb)) => {
                let mut diffs: Vec<CompDiff> = Vec::new();
                if self.unify_mode(&sa.input, &sb.input).is_err() {
                    diffs.push(CompDiff::Stdin {
                        expected: self.resolve_mode(&sa.input),
                        actual: self.resolve_mode(&sb.input),
                    });
                }
                if self.unify_mode(&sa.output, &sb.output).is_err() {
                    diffs.push(CompDiff::Stdout {
                        expected: self.resolve_mode(&sa.output),
                        actual: self.resolve_mode(&sb.output),
                    });
                }
                // A return-type disagreement folds into the rich `Return`
                // diff; exhausting the depth budget is resource exhaustion,
                // not a type disagreement, so it propagates verbatim.
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
                        expected: self.apply_return(&sa, &ta),
                        actual: self.apply_return(&sb, &tb),
                        diffs,
                    })
                }
            }
            (CompTy::Fun(a1, b1), CompTy::Fun(a2, b2)) => {
                self.unify_ty_inner(&a1, &a2, pairs, depth)?;
                self.unify_comp_ty_inner(&b1, &b2, pairs, depth)
            }
            (a, b) => Err(TypeErrorKind::CompTyMismatch {
                expected: a,
                actual: b,
                diffs: Vec::new(),
            }),
        }
    }

    /// Reconstruct a `CompTy::Return` after substitutions have been applied
    /// — used to render the post-resolution form for mismatch diagnostics.
    fn apply_return(&mut self, spec: &PipeSpec, ty: &Ty) -> CompTy {
        CompTy::Return(
            PipeSpec {
                input: self.resolve_mode(&spec.input),
                output: self.resolve_mode(&spec.output),
            },
            Box::new(self.apply_ty(ty)),
        )
    }

    /// Unify two pipeline modes under the equality rule of `docs/SPEC.md`
    /// §20.4: two variables unite, a variable and a ground mode bind, and
    /// two ground modes must be *equal*.  `None` and `Bytes` do not unify
    /// — a value edge cannot silently meet a byte edge (§4.2.1) — so a
    /// clash surfaces as a [`crate::mode::ModeMismatch`], which each caller
    /// maps onto its own diagnostic.
    ///
    /// # Errors
    /// Returns `Err` if the two modes are distinct ground modes — `None`
    /// against `Bytes`.
    pub fn unify_mode(
        &mut self,
        a: &PipeMode,
        b: &PipeMode,
    ) -> Result<(), crate::mode::ModeMismatch> {
        let a = self.resolve_mode(a);
        let b = self.resolve_mode(b);
        match (a, b) {
            (PipeMode::Var(ModeVar(va)), PipeMode::Var(ModeVar(vb))) => {
                self.modes.unite(va, vb);
                Ok(())
            }
            (PipeMode::Var(ModeVar(v)), g) | (g, PipeMode::Var(ModeVar(v))) => {
                let r = self.modes.find(v);
                self.modes.bind(r, g);
                Ok(())
            }
            (PipeMode::None, PipeMode::None) | (PipeMode::Bytes, PipeMode::Bytes) => Ok(()),
            (left, right) => Err(crate::mode::ModeMismatch { left, right }),
        }
    }

    /// Like [`Unifier::unify_row_inner`], walking the record spine is width:
    /// each field carries `depth`, and only matching a field *type* against
    /// the map element steps a level deeper.
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

/// Normalise a var-root pair into ascending order so that
/// `(a, b)` and `(b, a)` are stored under the same key in the
/// co-inductive guard set.
fn ordered_pair(a: u32, b: u32) -> (u32, u32) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Symmetric co-inductive obligation: two var roots of the same kind.
/// Returns `true` when the obligation is already in progress (or the
/// roots already coincide) — the caller discharges it as an immediate
/// success.  Shared by [`Unifier::unify_ty_inner`] and
/// [`Unifier::unify_comp_ty_inner`], which differ only in store and
/// variant kind.
fn guard_pair(seen: &mut HashSet<(u32, u32)>, a: u32, b: u32) -> bool {
    a == b || !seen.insert(ordered_pair(a, b))
}

/// One-sided co-inductive obligation: a var root against a finite
/// structural key of the other side.  Returns `true` when the obligation
/// is already in progress.  Generic over the key kind ([`TyKey`] /
/// [`CompTyKey`]) so the two unify methods share one set discipline.
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

    /// The `Step` value shape `Variant{`more: {head: String, tail:
    /// Thunk(tail)} | `done: Unit}`, whose tail thunk wraps `tail`.
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

    /// A value-var-anchored stream `T = Step(F T)` and a comp-var-anchored
    /// one `C = F Step(C)` are the *same* equi-recursive type under
    /// different anchors.  Unifying `T` with `Step(C)` must succeed and
    /// terminate: it used to recurse until the stack overflowed, because the
    /// co-inductive guard fired only on `Var`/`Var` pairs and here the
    /// recursion always re-enters as `Var`-vs-concrete-structure.
    #[test]
    fn unifies_value_anchored_and_comp_anchored_stream() {
        let mut u = Unifier::new();

        // T := Step(F T), with F T = Return(pure, T).
        let t = u.fresh_tyvar();
        let t_root = u.ty_root(t.0);
        let t_body = step(CompTy::pure(Ty::Var(t)));
        u.bind_ty_root(t_root, t_body);

        // C := F Step(C) = Return(pure, Step(C)).
        let CompTy::Var(CompTyVar(c_root)) = u.fresh_comp_ty() else {
            unreachable!("fresh_comp_ty yields a Var")
        };
        let step_c = step(CompTy::Var(CompTyVar(c_root)));
        u.bind_comp_root(c_root, CompTy::pure(step_c.clone()));

        u.unify_ty(&Ty::Var(t), &step_c)
            .expect("equi-recursive stream types unify regardless of anchor");
    }

    /// A `List(List(… Int …))` spine nested past [`MAX_UNIFY_DEPTH`].  With
    /// no var root for the co-inductive guard to memoize, every structural
    /// path descends to a depth bounded only by term size.
    fn deep_list(n: u32) -> Ty {
        let mut ty = Ty::Int;
        for _ in 0..n {
            ty = Ty::List(Box::new(ty));
        }
        ty
    }

    /// Run a deep-type assertion on a thread with a generous stack.  A
    /// `Box`-chained `Ty` deep enough to *trip the bound under test* is also
    /// deep enough that merely constructing and recursively `Drop`ping it —
    /// an inherent property of the nested `Box<Ty>`, independent of the
    /// unifier — sits near the default 2 MiB test-thread ceiling.  The
    /// larger stack keeps construction and teardown clear of that ceiling so
    /// the only thing on trial is whether the unifier returns
    /// [`TypeErrorKind::TypeTooDeep`] instead of overflowing.
    fn on_deep_stack(body: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(body)
            .expect("spawn deep-stack thread")
            .join()
            .expect("deep-stack assertion");
    }

    /// A structurally deep, *variable-free* type unified against its twin
    /// drives the [`Unifier::unify_ty_inner`] `List` arm past the ceiling;
    /// the defensive bound must turn that would-be stack overflow into a
    /// graceful [`TypeErrorKind::TypeTooDeep`] rather than aborting.
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

    /// The one-sided obligation builds a [`TyKey`] of the concrete side: a
    /// ty-var unified against a deep concrete type fingerprints it through
    /// [`Unifier::ty_key`], a structural recursion *outside* the unify arms.
    /// That path is bounded too, so it reports [`TypeErrorKind::TypeTooDeep`]
    /// rather than overflowing.
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

    /// Binding a row variable runs the occurs check ([`Unifier::row_occurs`])
    /// over the candidate row, descending through each field *type* — another
    /// structural recursion outside the unify arms.  A field whose type nests
    /// past the ceiling must surface [`TypeErrorKind::TypeTooDeep`] there, not
    /// overflow the stack.
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

    /// Build a closed record row from `(label, ty)` pairs in order.
    fn record_row(fields: &[(&str, Ty)]) -> Row {
        fields.iter().rev().fold(Row::Empty, |rest, (l, t)| {
            Row::Extend((*l).into(), Box::new(t.clone()), Box::new(rest))
        })
    }

    /// Open a row at `tail` after the given fields.
    fn open_row(fields: &[(&str, Ty)], tail: RowVar) -> Row {
        fields.iter().rev().fold(Row::Var(tail), |rest, (l, t)| {
            Row::Extend((*l).into(), Box::new(t.clone()), Box::new(rest))
        })
    }

    /// Resolve a row to a label→type map for assertions.
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

    /// A *wide* record — many sibling fields, each shallow — must unify
    /// freely: iterating across the row spine is width, not nesting depth,
    /// so a field count far past [`MAX_UNIFY_DEPTH`] never charges against
    /// the defensive ceiling.  This pins the no-false-positive property: a
    /// per-field-recursion bound would reject this valid record as
    /// [`TypeErrorKind::TypeTooDeep`].
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

    /// The same width property for a variant row: a tag union with far more
    /// than [`MAX_UNIFY_DEPTH`] alternatives unifies, since the alternatives
    /// are siblings (width), not a nesting chain (depth).
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

    /// The Rémy rewrite unifies records whose fields appear in a different
    /// order: each label is matched to its same-label partner regardless of
    /// spine position, and the per-field types are unified.
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

    /// The Rémy rewrite under an open tail (the shadowing case): unifying an
    /// open row `{a: Int | ρ}` against a closed `{a: Int, b: String}` binds the
    /// tail ρ to carry the extra field `b`.
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

    /// Two rows that disagree at the head while sharing the *same* tail
    /// variable (`{x: Int | ρ} ≐ {y: Int | ρ}`) have no finite or rational
    /// solution.  The scoped-labels side condition reports it instead of
    /// diverging through the Rémy rewrite.
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

    /// The same shared-tail divergence with the disagreement *below* the head
    /// (`{x, a | ρ} ≐ {y, b | ρ}`): the immediate rests are `Extend` nodes, not
    /// the shared variable, so a depth-1 guard misses it and the Rémy rewrite
    /// recurses until the stack overflows.  Walking the whole spine catches it.
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

    /// A shared tail with the *same* label multiset in a different order
    /// (`{a, b | ρ} ≐ {b, a | ρ}`) is a permutation, not a divergence: it has a
    /// solution and the spine guard must let the Rémy rewrite proceed.
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

    /// A row cycle that passes through a *field type* (`ρ ≐ {x: {n: Int | ρ}}`)
    /// denotes an infinite record.  The occurs check descends into field types
    /// and rejects it rather than installing the cyclic binding.
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

    /// The pure unifier matches a probe `{x: α | ρ}` against the *head*
    /// occurrence of a duplicated-label spine `{x: Int, x: String}`: row
    /// unification walks the spine head-first.  Duplicate explicit keys are
    /// resolved last-wins upstream in `infer_map_val` (which dedups before
    /// building the row), so the unifier never receives a duplicated-label
    /// record literal in practice; this records the spine-walk behaviour.
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
