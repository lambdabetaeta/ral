//! Union-find unifier over four variable kinds: type, computation type, mode, row.
//!
//! A single `Store<T>` handles the union-find plumbing for any payload type T.
//! Kind-specific unify methods on `Unifier` encode structural rules (including
//! the coercions preserved by design: Unit↔String, Unit↔Bytes, Record↔Map, None↔Bytes).
//!
//! Computation types are *equi-recursive*: a binding such as
//! `comp_ty_slots[N] = Bound(Fun(Int, Var(N)))` is allowed and represents a
//! self-referential type.  Every traversal that descends through `Thunk` /
//! `Fun` / `Return` carries a `visited: HashSet<u32>` of comp-var roots in
//! current expansion so a cycle returns a back-edge instead of recursing
//! forever.  Unification carries a co-inductive `HashSet<(u32, u32)>` of
//! pairs already in progress, so unifying two cyclic types reaches a
//! fixed-point instead of looping.  Value types remain non-recursive — the
//! occurs check on `TyVar` is preserved.

use super::scheme::{CompDiff, TypeErrorKind};
use super::ty::{CompTy, CompTyVar, ModeVar, PipeMode, PipeSpec, Row, RowVar, Ty, TyVar};
use std::collections::HashSet;

/// Tag for the value-type occurs check.  The comp-type occurs check is gone
/// (computation types are equi-recursive), so this only carries a `TyVar`.
/// Wrapped to keep the existing call shape and to leave room for a future
/// reintroduction of comp-var checks should the recursive-type semantics
/// ever change.
#[derive(Clone, Copy)]
struct VarTag(TyVar);

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

impl<T: Clone> Store<T> {
    fn new() -> Self {
        Store {
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
        Unifier {
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

    /// Allocate a fresh comp-var slot and return its root id.  Used by
    /// scheme instantiation to mint independent slots for each cyclic
    /// comp-var in a polymorphic recursive scheme.
    pub fn fresh_comp_root(&mut self) -> u32 {
        self.ctys.fresh()
    }

    /// Bind a freshly-minted comp-var root to a `CompTy` value.  Pairs
    /// with `fresh_comp_root` for instantiation: the binding is the
    /// scheme's snapshot rewritten through the substitution map.
    pub fn bind_comp_root(&mut self, root: u32, value: CompTy) {
        self.ctys.bind(root, value);
    }

    /// If the comp-var root has a non-Var binding, return its current
    /// (resolved, applied) value; otherwise `None`.  Used by
    /// `generalize` to snapshot cyclic comp-var bindings for storage
    /// in the scheme.
    pub fn resolved_comp_root_binding(&mut self, root: u32) -> Option<CompTy> {
        match self.ctys.get(root) {
            Some(CompTy::Var(_)) | None => None,
            Some(other) => Some(self.apply_comp_ty(&other)),
        }
    }

    /// Collect comp-var roots that appear as cycle back-edges under `ty`.
    ///
    /// The input is applied first, so any cyclic computation type appears as a
    /// `CompTy::Var(root)` node at the back-edge.  Unbound comp vars are
    /// ignored; only roots with an existing binding are returned.
    pub fn cyclic_comp_roots_in_ty(&mut self, ty: &Ty) -> Vec<u32> {
        let applied = self.apply_ty(ty);
        let mut roots = HashSet::new();
        self.collect_cyclic_comp_roots_ty(&applied, &mut roots);
        let mut out: Vec<u32> = roots.into_iter().collect();
        out.sort_unstable();
        out
    }

    // ── Resolve: follow variable chains to canonical form ────────────────────

    pub fn resolve_ty(&mut self, ty: &Ty) -> Ty {
        if let Ty::Var(TyVar(i)) = ty {
            match self.tys.get(*i) {
                Some(t) => self.resolve_ty(&t),
                None => Ty::Var(TyVar(self.tys.find(*i))),
            }
        } else {
            ty.clone()
        }
    }

    pub fn resolve_comp_ty(&mut self, cty: &CompTy) -> CompTy {
        if let CompTy::Var(CompTyVar(i)) = cty {
            match self.ctys.get(*i) {
                Some(t) => self.resolve_comp_ty(&t),
                None => CompTy::Var(CompTyVar(self.ctys.find(*i))),
            }
        } else {
            cty.clone()
        }
    }

    pub fn resolve_mode(&mut self, mode: &PipeMode) -> PipeMode {
        if let PipeMode::Var(ModeVar(i)) = mode {
            match self.modes.get(*i) {
                Some(g) => self.resolve_mode(&g),
                None => PipeMode::Var(ModeVar(self.modes.find(*i))),
            }
        } else {
            mode.clone()
        }
    }

    /// Follow row variable bindings to canonical form.
    /// The returned row may still contain nested unresolved variables.
    pub fn resolve_row(&mut self, row: &Row) -> Row {
        if let Row::Var(RowVar(i)) = row {
            match self.rows.get(*i) {
                Some(b) => self.resolve_row(&b),
                None => Row::Var(RowVar(self.rows.find(*i))),
            }
        } else {
            row.clone()
        }
    }

    // ── Apply: recursively substitute all variables ──────────────────────────

    pub fn apply_ty(&mut self, ty: &Ty) -> Ty {
        let mut visited = HashSet::new();
        self.apply_ty_inner(ty, &mut visited)
    }

    pub fn apply_comp_ty(&mut self, cty: &CompTy) -> CompTy {
        let mut visited = HashSet::new();
        self.apply_comp_ty_inner(cty, &mut visited)
    }

    pub fn apply_mode(&mut self, mode: &PipeMode) -> PipeMode {
        let mut visited = HashSet::new();
        self.apply_mode_inner(mode, &mut visited)
    }

    pub fn apply_row(&mut self, row: &Row) -> Row {
        let mut visited = HashSet::new();
        self.apply_row_inner(row, &mut visited)
    }

    fn apply_ty_inner(&mut self, ty: &Ty, visited: &mut HashSet<u32>) -> Ty {
        match self.resolve_ty(ty) {
            Ty::List(a) => Ty::List(Box::new(self.apply_ty_inner(&a, visited))),
            Ty::Map(a) => Ty::Map(Box::new(self.apply_ty_inner(&a, visited))),
            Ty::Handle(a) => Ty::Handle(Box::new(self.apply_ty_inner(&a, visited))),
            Ty::Record(r) => Ty::Record(self.apply_row_inner(&r, visited)),
            Ty::Variant(r) => Ty::Variant(self.apply_row_inner(&r, visited)),
            Ty::Thunk(b) => Ty::Thunk(Box::new(self.apply_comp_ty_inner(&b, visited))),
            other => other,
        }
    }

    fn apply_comp_ty_inner(&mut self, cty: &CompTy, visited: &mut HashSet<u32>) -> CompTy {
        // If the input is a comp var that resolves to a non-Var binding we
        // are about to expand, mark its root visited so a back-edge to the
        // same root short-circuits to `Var(root)` instead of looping.
        let root = match cty {
            CompTy::Var(CompTyVar(i)) => Some(self.ctys.find(*i)),
            _ => None,
        };
        if let Some(r) = root
            && visited.contains(&r)
        {
            return CompTy::Var(CompTyVar(r));
        }
        let resolved = self.resolve_comp_ty(cty);
        if matches!(&resolved, CompTy::Var(_)) {
            return resolved;
        }
        if let Some(r) = root {
            visited.insert(r);
        }
        match resolved {
            CompTy::Return(spec, a) => CompTy::Return(
                PipeSpec {
                    input: self.apply_mode_inner(&spec.input, visited),
                    output: self.apply_mode_inner(&spec.output, visited),
                },
                Box::new(self.apply_ty_inner(&a, visited)),
            ),
            CompTy::Fun(a, b) => CompTy::Fun(
                Box::new(self.apply_ty_inner(&a, visited)),
                Box::new(self.apply_comp_ty_inner(&b, visited)),
            ),
            CompTy::Var(_) => unreachable!(),
        }
    }

    fn apply_mode_inner(&mut self, mode: &PipeMode, _visited: &mut HashSet<u32>) -> PipeMode {
        self.resolve_mode(mode)
    }

    fn apply_row_inner(&mut self, row: &Row, visited: &mut HashSet<u32>) -> Row {
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

    fn collect_cyclic_comp_roots_ty(&mut self, ty: &Ty, out: &mut HashSet<u32>) {
        match self.resolve_ty(ty) {
            Ty::List(a) | Ty::Map(a) | Ty::Handle(a) => self.collect_cyclic_comp_roots_ty(&a, out),
            Ty::Record(r) | Ty::Variant(r) => self.collect_cyclic_comp_roots_row(&r, out),
            Ty::Thunk(b) => self.collect_cyclic_comp_roots_comp(&b, out),
            _ => {}
        }
    }

    fn collect_cyclic_comp_roots_row(&mut self, row: &Row, out: &mut HashSet<u32>) {
        match self.resolve_row(row) {
            Row::Empty | Row::Var(_) => {}
            Row::Extend(_, ty, rest) => {
                self.collect_cyclic_comp_roots_ty(&ty, out);
                self.collect_cyclic_comp_roots_row(&rest, out);
            }
        }
    }

    fn collect_cyclic_comp_roots_comp(&mut self, cty: &CompTy, out: &mut HashSet<u32>) {
        match cty {
            CompTy::Var(CompTyVar(i)) => {
                let root = self.comp_root(*i);
                if self.resolved_comp_root_binding(root).is_some() {
                    out.insert(root);
                }
            }
            CompTy::Return(_, ty) => self.collect_cyclic_comp_roots_ty(ty, out),
            CompTy::Fun(a, b) => {
                self.collect_cyclic_comp_roots_ty(a, out);
                self.collect_cyclic_comp_roots_comp(b, out);
            }
        }
    }

    // ── Occurs checks ────────────────────────────────────────────────────────
    //
    // VarTag unifies TyVar and comp-var (u32) checks into one traversal family.
    // RowVar gets its own function since it only walks the row spine (row vars
    // appear in row position, not inside field types).

    fn occurs_ty(&mut self, v: VarTag, ty: &Ty) -> bool {
        let mut visited = HashSet::new();
        self.occurs_ty_inner(v, ty, &mut visited)
    }

    fn occurs_ty_inner(&mut self, v: VarTag, ty: &Ty, visited: &mut HashSet<u32>) -> bool {
        match self.resolve_ty(ty) {
            Ty::Var(u) => v.0 == u,
            Ty::List(a) | Ty::Map(a) | Ty::Handle(a) => self.occurs_ty_inner(v, &a, visited),
            Ty::Record(r) | Ty::Variant(r) => self.occurs_row_fields(v, &r, visited),
            Ty::Thunk(b) => self.occurs_comp_inner(v, &b, visited),
            _ => false,
        }
    }

    fn occurs_row_fields(&mut self, v: VarTag, row: &Row, visited: &mut HashSet<u32>) -> bool {
        match self.resolve_row(row) {
            Row::Empty | Row::Var(_) => false,
            Row::Extend(_, ty, rest) => {
                self.occurs_ty_inner(v, &ty, visited) || self.occurs_row_fields(v, &rest, visited)
            }
        }
    }

    fn occurs_comp_inner(
        &mut self,
        v: VarTag,
        cty: &CompTy,
        visited: &mut HashSet<u32>,
    ) -> bool {
        let root = match cty {
            CompTy::Var(CompTyVar(i)) => Some(self.ctys.find(*i)),
            _ => None,
        };
        if let Some(r) = root
            && visited.contains(&r)
        {
            // Already searched this subtree.  Recursive types are accepted;
            // the search returns false on revisit (no new occurrence found
            // beyond what is already known).
            return false;
        }
        let resolved = self.resolve_comp_ty(cty);
        if matches!(&resolved, CompTy::Var(_)) {
            // An unbound comp var holds no information about TyVar
            // occurrence — value-type occurs is what we care about.
            return false;
        }
        if let Some(r) = root {
            visited.insert(r);
        }
        match resolved {
            CompTy::Return(spec, a) => {
                self.occurs_ty_inner(v, &a, visited)
                    || self.occurs_mode_inner(v, &spec.input, visited)
                    || self.occurs_mode_inner(v, &spec.output, visited)
            }
            CompTy::Fun(a, b) => {
                self.occurs_ty_inner(v, &a, visited) || self.occurs_comp_inner(v, &b, visited)
            }
            CompTy::Var(_) => unreachable!(),
        }
    }

    fn occurs_mode_inner(
        &mut self,
        _v: VarTag,
        _mode: &PipeMode,
        _visited: &mut HashSet<u32>,
    ) -> bool {
        false
    }

    fn row_occurs(&mut self, v: RowVar, row: &Row) -> bool {
        match self.resolve_row(row) {
            Row::Empty => false,
            Row::Var(u) => u == v,
            Row::Extend(_, _, r) => self.row_occurs(v, &r),
        }
    }

    // ── Unification ──────────────────────────────────────────────────────────

    pub fn unify_ty(&mut self, a: &Ty, b: &Ty) -> Result<(), TypeErrorKind> {
        let mut pairs = HashSet::new();
        self.unify_ty_inner(a, b, &mut pairs)
    }

    fn unify_ty_inner(
        &mut self,
        a: &Ty,
        b: &Ty,
        pairs: &mut HashSet<(u32, u32)>,
    ) -> Result<(), TypeErrorKind> {
        let a = self.resolve_ty(a);
        let b = self.resolve_ty(b);

        if let (Ty::Var(TyVar(ai)), Ty::Var(TyVar(bi))) = (&a, &b) {
            self.tys.unite(*ai, *bi);
            return Ok(());
        }
        if let Ty::Var(TyVar(vi)) = &a {
            let vi = *vi;
            // The TyVar occurs check is preserved — value types remain
            // non-recursive even though comp types may cycle.  The traversal
            // is cycle-aware so it terminates when walking through a Thunk
            // whose body is recursive.
            if self.occurs_ty(VarTag(TyVar(vi)), &b) {
                return Err(TypeErrorKind::RecursiveType);
            }
            let r = self.tys.find(vi);
            self.tys.bind(r, b);
            return Ok(());
        }
        if let Ty::Var(TyVar(vi)) = &b {
            let vi = *vi;
            if self.occurs_ty(VarTag(TyVar(vi)), &a) {
                return Err(TypeErrorKind::RecursiveType);
            }
            let r = self.tys.find(vi);
            self.tys.bind(r, a);
            return Ok(());
        }
        match (a, b) {
            (Ty::Unit, Ty::Unit)
            | (Ty::Bool, Ty::Bool)
            | (Ty::Int, Ty::Int)
            | (Ty::Float, Ty::Float)
            | (Ty::String, Ty::String)
            | (Ty::Bytes, Ty::Bytes) => Ok(()),
            (Ty::List(a1), Ty::List(b1)) => self.unify_ty_inner(&a1, &b1, pairs),
            (Ty::Map(a1), Ty::Map(b1)) => self.unify_ty_inner(&a1, &b1, pairs),
            (Ty::Handle(a1), Ty::Handle(b1)) => self.unify_ty_inner(&a1, &b1, pairs),
            (Ty::Record(r1), Ty::Record(r2)) => self.unify_row_inner(&r1, &r2, pairs),
            (Ty::Variant(r1), Ty::Variant(r2)) => self.unify_row_inner(&r1, &r2, pairs),
            (Ty::Thunk(a1), Ty::Thunk(b1)) => self.unify_comp_ty_inner(&a1, &b1, pairs),
            // Record ↔ Map coercion: a record can be used where a homogeneous map
            // is expected if all its field types unify to the map's element type.
            (Ty::Map(elem), Ty::Record(row)) => self.unify_map_record(&elem, &row, pairs),
            (Ty::Record(row), Ty::Map(elem)) => self.unify_map_record(&elem, &row, pairs),
            (a, b) => Err(TypeErrorKind::TyMismatch {
                expected: a,
                actual: b,
            }),
        }
    }

    /// Row unification using the Rémy rewrite rule.
    pub fn unify_row(&mut self, a: &Row, b: &Row) -> Result<(), TypeErrorKind> {
        let mut pairs = HashSet::new();
        self.unify_row_inner(a, b, &mut pairs)
    }

    fn unify_row_inner(
        &mut self,
        a: &Row,
        b: &Row,
        pairs: &mut HashSet<(u32, u32)>,
    ) -> Result<(), TypeErrorKind> {
        let a = self.resolve_row(a);
        let b = self.resolve_row(b);

        if let (Row::Var(RowVar(ai)), Row::Var(RowVar(bi))) = (&a, &b) {
            self.rows.unite(*ai, *bi);
            return Ok(());
        }
        if let Row::Var(RowVar(vi)) = &a {
            let vi = *vi;
            if self.row_occurs(RowVar(vi), &b) {
                return Err(TypeErrorKind::RecursiveRow);
            }
            let r = self.rows.find(vi);
            self.rows.bind(r, b);
            return Ok(());
        }
        if let Row::Var(RowVar(vi)) = &b {
            let vi = *vi;
            if self.row_occurs(RowVar(vi), &a) {
                return Err(TypeErrorKind::RecursiveRow);
            }
            let r = self.rows.find(vi);
            self.rows.bind(r, a);
            return Ok(());
        }

        match (a, b) {
            (Row::Empty, Row::Empty) => Ok(()),
            (Row::Empty, Row::Extend(l, _, _)) => Err(TypeErrorKind::RowExtraField { label: l }),
            (Row::Extend(l, _, _), Row::Empty) => Err(TypeErrorKind::RowMissingField { label: l }),
            (Row::Extend(l1, t1, r1), Row::Extend(l2, t2, r2)) => {
                if l1 == l2 {
                    let (t1, t2) = (*t1, *t2);
                    self.unify_ty_inner(&t1, &t2, pairs)?;
                    self.unify_row_inner(&r1, &r2, pairs)
                } else {
                    // Reject mixed-alphabet rows: tag labels (`.l`) and bare
                    // labels (`l`) are disjoint by design — a record literal
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
                    let rho = self.fresh_row_var();
                    let new_r1 = Row::Extend(l2.clone(), t2.clone(), Box::new(Row::Var(rho)));
                    let new_r2 = Row::Extend(l1.clone(), t1.clone(), Box::new(Row::Var(rho)));
                    self.unify_row_inner(&r1, &new_r1, pairs)?;
                    self.unify_row_inner(&r2, &new_r2, pairs)
                }
            }
            _ => unreachable!(),
        }
    }

    pub fn unify_comp_ty(&mut self, a: &CompTy, b: &CompTy) -> Result<(), TypeErrorKind> {
        let mut pairs = HashSet::new();
        self.unify_comp_ty_inner(a, b, &mut pairs)
    }

    fn unify_comp_ty_inner(
        &mut self,
        a: &CompTy,
        b: &CompTy,
        pairs: &mut HashSet<(u32, u32)>,
    ) -> Result<(), TypeErrorKind> {
        // Co-inductive guard: if we re-enter unification on the same pair of
        // comp-var roots, treat as already unified.  This is what makes
        // unifying two cyclic comp types terminate.
        if let (CompTy::Var(CompTyVar(ai)), CompTy::Var(CompTyVar(bi))) = (a, b) {
            let ar = self.ctys.find(*ai);
            let br = self.ctys.find(*bi);
            if ar == br {
                return Ok(());
            }
            let p = ordered_pair(ar, br);
            if pairs.contains(&p) {
                return Ok(());
            }
            pairs.insert(p);
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
        match (a, b) {
            (CompTy::Return(sa, ta), CompTy::Return(sb, tb)) => {
                let mut diffs: Vec<CompDiff> = Vec::new();
                if self.unify_mode_inner(&sa.input, &sb.input, pairs).is_err() {
                    diffs.push(CompDiff::Stdin {
                        expected: self.apply_mode(&sa.input),
                        actual: self.apply_mode(&sb.input),
                    });
                }
                if self.unify_mode_inner(&sa.output, &sb.output, pairs).is_err() {
                    diffs.push(CompDiff::Stdout {
                        expected: self.apply_mode(&sa.output),
                        actual: self.apply_mode(&sb.output),
                    });
                }
                if self.unify_ty_inner(&ta, &tb, pairs).is_err() {
                    diffs.push(CompDiff::ReturnType {
                        expected: self.apply_ty(&ta),
                        actual: self.apply_ty(&tb),
                    });
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
                self.unify_ty_inner(&a1, &a2, pairs)?;
                self.unify_comp_ty_inner(&b1, &b2, pairs)
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
                input: self.apply_mode(&spec.input),
                output: self.apply_mode(&spec.output),
            },
            Box::new(self.apply_ty(ty)),
        )
    }

    pub fn unify_mode(&mut self, a: &PipeMode, b: &PipeMode) -> Result<(), TypeErrorKind> {
        let mut pairs = HashSet::new();
        self.unify_mode_inner(a, b, &mut pairs)
    }

    fn unify_mode_inner(
        &mut self,
        a: &PipeMode,
        b: &PipeMode,
        _pairs: &mut HashSet<(u32, u32)>,
    ) -> Result<(), TypeErrorKind> {
        let a = self.resolve_mode(a);
        let b = self.resolve_mode(b);

        if let (PipeMode::Var(ModeVar(ai)), PipeMode::Var(ModeVar(bi))) = (&a, &b) {
            self.modes.unite(*ai, *bi);
            return Ok(());
        }
        if let PipeMode::Var(ModeVar(vi)) = &a {
            let r = self.modes.find(*vi);
            self.modes.bind(r, b);
            return Ok(());
        }
        if let PipeMode::Var(ModeVar(vi)) = &b {
            let r = self.modes.find(*vi);
            self.modes.bind(r, a);
            return Ok(());
        }
        match (a, b) {
            (PipeMode::None, PipeMode::None) | (PipeMode::Bytes, PipeMode::Bytes) => Ok(()),
            // None ↔ Bytes coercion: preserved by design.
            (PipeMode::None, PipeMode::Bytes) | (PipeMode::Bytes, PipeMode::None) => Ok(()),
            (a, b) => Err(TypeErrorKind::ModeMismatch {
                expected: a,
                actual: b,
            }),
        }
    }

    fn unify_map_record(
        &mut self,
        elem: &Ty,
        row: &Row,
        pairs: &mut HashSet<(u32, u32)>,
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
                self.unify_ty_inner(&ty, elem, pairs)?;
                self.unify_map_record(elem, &rest, pairs)
            }
        }
    }
}

impl Default for Unifier {
    fn default() -> Self {
        Self::new()
    }
}

/// True if `label` is a tag label (begins with `.`).  Bare-keyed and
/// tag-keyed rows do not unify: keeping the alphabets disjoint at every
/// row-extend swap is what prevents `[host: String, .ok: Int]` from ever
/// typechecking.
fn is_tag_label(label: &str) -> bool {
    label.starts_with('.')
}

/// Normalise a comp-var-root pair into ascending order so that
/// `(a, b)` and `(b, a)` are stored under the same key in the
/// co-inductive guard set.
fn ordered_pair(a: u32, b: u32) -> (u32, u32) {
    if a <= b { (a, b) } else { (b, a) }
}
