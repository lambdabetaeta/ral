//! Union-find unifier over four variable kinds: type, computation type, mode, row.
//!
//! A single `Store<T>` handles the union-find plumbing for any payload type T.
//! Kind-specific unify methods on `Unifier` encode structural rules (including
//! the coercions preserved by design: Unit↔String, Unit↔Bytes, Record↔Map, None↔Bytes).

use super::scheme::{CompDiff, TypeErrorKind};
use super::ty::{CompTy, CompTyVar, ModeVar, PipeMode, PipeSpec, Row, RowVar, Ty, TyVar};

/// Discriminates between the two variable kinds whose occurs checks share
/// the same structural traversal.  `RowVar` is handled separately since it
/// only walks the row spine, not field types.
#[derive(Clone, Copy)]
enum VarTag {
    Ty(TyVar),
    Comp(CompTyVar),
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
        match self.resolve_ty(ty) {
            Ty::List(a) => Ty::List(Box::new(self.apply_ty(&a))),
            Ty::Map(a) => Ty::Map(Box::new(self.apply_ty(&a))),
            Ty::Handle(a) => Ty::Handle(Box::new(self.apply_ty(&a))),
            Ty::Record(r) => Ty::Record(self.apply_row(&r)),
            Ty::Thunk(b) => Ty::Thunk(Box::new(self.apply_comp_ty(&b))),
            other => other,
        }
    }

    pub fn apply_comp_ty(&mut self, cty: &CompTy) -> CompTy {
        match self.resolve_comp_ty(cty) {
            CompTy::Return(spec, a) => self.apply_return(&spec, &a),
            CompTy::Fun(a, b) => CompTy::Fun(
                Box::new(self.apply_ty(&a)),
                Box::new(self.apply_comp_ty(&b)),
            ),
            other => other,
        }
    }

    pub fn apply_mode(&mut self, mode: &PipeMode) -> PipeMode {
        match self.resolve_mode(mode) {
            PipeMode::Values(a) => PipeMode::Values(Box::new(self.apply_ty(&a))),
            other => other,
        }
    }

    pub fn apply_row(&mut self, row: &Row) -> Row {
        match self.resolve_row(row) {
            Row::Empty => Row::Empty,
            Row::Var(v) => Row::Var(v),
            Row::Extend(l, ty, rest) => {
                let ty2 = self.apply_ty(&ty);
                let rest2 = self.apply_row(&rest);
                Row::Extend(l, Box::new(ty2), Box::new(rest2))
            }
        }
    }

    // ── Occurs checks ────────────────────────────────────────────────────────
    //
    // VarTag unifies TyVar and comp-var (u32) checks into one traversal family.
    // RowVar gets its own function since it only walks the row spine (row vars
    // appear in row position, not inside field types).

    fn occurs_ty(&mut self, v: VarTag, ty: &Ty) -> bool {
        match self.resolve_ty(ty) {
            Ty::Var(u) => matches!(v, VarTag::Ty(t) if t == u),
            Ty::List(a) | Ty::Map(a) | Ty::Handle(a) => self.occurs_ty(v, &a),
            Ty::Record(r) => self.occurs_row_fields(v, &r),
            Ty::Thunk(b) => self.occurs_comp(v, &b),
            _ => false,
        }
    }

    fn occurs_row_fields(&mut self, v: VarTag, row: &Row) -> bool {
        match self.resolve_row(row) {
            Row::Empty | Row::Var(_) => false,
            Row::Extend(_, ty, rest) => self.occurs_ty(v, &ty) || self.occurs_row_fields(v, &rest),
        }
    }

    fn occurs_comp(&mut self, v: VarTag, cty: &CompTy) -> bool {
        match self.resolve_comp_ty(cty) {
            CompTy::Var(cv) => matches!(v, VarTag::Comp(c) if c == cv),
            CompTy::Return(spec, a) => {
                // Modes contain only Ty, not comp vars — skip mode check for VarTag::Comp.
                self.occurs_ty(v, &a)
                    || matches!(v, VarTag::Ty(_))
                        && (self.occurs_mode(v, &spec.input) || self.occurs_mode(v, &spec.output))
            }
            CompTy::Fun(a, b) => self.occurs_ty(v, &a) || self.occurs_comp(v, &b),
        }
    }

    fn occurs_mode(&mut self, v: VarTag, mode: &PipeMode) -> bool {
        match self.resolve_mode(mode) {
            PipeMode::Values(a) => self.occurs_ty(v, &a),
            _ => false,
        }
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
        let a = self.resolve_ty(a);
        let b = self.resolve_ty(b);

        if let (Ty::Var(TyVar(ai)), Ty::Var(TyVar(bi))) = (&a, &b) {
            self.tys.unite(*ai, *bi);
            return Ok(());
        }
        if let Ty::Var(TyVar(vi)) = &a {
            let vi = *vi;
            if self.occurs_ty(VarTag::Ty(TyVar(vi)), &b) {
                return Err(TypeErrorKind::RecursiveType);
            }
            let r = self.tys.find(vi);
            self.tys.bind(r, b);
            return Ok(());
        }
        if let Ty::Var(TyVar(vi)) = &b {
            let vi = *vi;
            if self.occurs_ty(VarTag::Ty(TyVar(vi)), &a) {
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
            (Ty::List(a1), Ty::List(b1)) => self.unify_ty(&a1, &b1),
            (Ty::Map(a1), Ty::Map(b1)) => self.unify_ty(&a1, &b1),
            (Ty::Handle(a1), Ty::Handle(b1)) => self.unify_ty(&a1, &b1),
            (Ty::Record(r1), Ty::Record(r2)) => self.unify_row(&r1, &r2),
            (Ty::Thunk(a1), Ty::Thunk(b1)) => self.unify_comp_ty(&a1, &b1),
            // Record ↔ Map coercion: a record can be used where a homogeneous map
            // is expected if all its field types unify to the map's element type.
            (Ty::Map(elem), Ty::Record(row)) => self.unify_map_record(&elem, &row),
            (Ty::Record(row), Ty::Map(elem)) => self.unify_map_record(&elem, &row),
            (a, b) => Err(TypeErrorKind::TyMismatch {
                expected: a,
                actual: b,
            }),
        }
    }

    /// Row unification using the Rémy rewrite rule.
    pub fn unify_row(&mut self, a: &Row, b: &Row) -> Result<(), TypeErrorKind> {
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
                    self.unify_ty(&t1, &t2)?;
                    self.unify_row(&r1, &r2)
                } else {
                    let rho = self.fresh_row_var();
                    let new_r1 = Row::Extend(l2.clone(), t2.clone(), Box::new(Row::Var(rho)));
                    let new_r2 = Row::Extend(l1.clone(), t1.clone(), Box::new(Row::Var(rho)));
                    self.unify_row(&r1, &new_r1)?;
                    self.unify_row(&r2, &new_r2)
                }
            }
            _ => unreachable!(),
        }
    }

    pub fn unify_comp_ty(&mut self, a: &CompTy, b: &CompTy) -> Result<(), TypeErrorKind> {
        let a = self.resolve_comp_ty(a);
        let b = self.resolve_comp_ty(b);

        if let (CompTy::Var(CompTyVar(ai)), CompTy::Var(CompTyVar(bi))) = (&a, &b) {
            self.ctys.unite(*ai, *bi);
            return Ok(());
        }
        if let CompTy::Var(CompTyVar(vi)) = &a {
            let vi = *vi;
            if self.occurs_comp(VarTag::Comp(CompTyVar(vi)), &b) {
                return Err(TypeErrorKind::RecursiveCompTy);
            }
            let r = self.ctys.find(vi);
            self.ctys.bind(r, b);
            return Ok(());
        }
        if let CompTy::Var(CompTyVar(vi)) = &b {
            let vi = *vi;
            if self.occurs_comp(VarTag::Comp(CompTyVar(vi)), &a) {
                return Err(TypeErrorKind::RecursiveCompTy);
            }
            let r = self.ctys.find(vi);
            self.ctys.bind(r, a);
            return Ok(());
        }
        match (a, b) {
            (CompTy::Return(sa, ta), CompTy::Return(sb, tb)) => {
                let mut diffs: Vec<CompDiff> = Vec::new();
                if self.unify_mode(&sa.input, &sb.input).is_err() {
                    diffs.push(CompDiff::Stdin {
                        expected: self.apply_mode(&sa.input),
                        actual: self.apply_mode(&sb.input),
                    });
                }
                if self.unify_mode(&sa.output, &sb.output).is_err() {
                    diffs.push(CompDiff::Stdout {
                        expected: self.apply_mode(&sa.output),
                        actual: self.apply_mode(&sb.output),
                    });
                }
                if self.unify_ty(&ta, &tb).is_err() {
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
                self.unify_ty(&a1, &a2)?;
                self.unify_comp_ty(&b1, &b2)
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
            (PipeMode::Values(ta), PipeMode::Values(tb)) => self.unify_ty(&ta, &tb),
            (a, b) => Err(TypeErrorKind::ModeMismatch {
                expected: a,
                actual: b,
            }),
        }
    }

    fn unify_map_record(&mut self, elem: &Ty, row: &Row) -> Result<(), TypeErrorKind> {
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
                self.unify_ty(&ty, elem)?;
                self.unify_map_record(elem, &rest)
            }
        }
    }
}

impl Default for Unifier {
    fn default() -> Self {
        Self::new()
    }
}
