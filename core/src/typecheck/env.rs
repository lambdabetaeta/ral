//! Typing environment and inference context, plus free-variable collection.

use super::scheme::{Scheme, TypeError, TypeErrorKind};
use super::ty::{CompTy, ModeVar, PipeMode, Row, RowVar, Ty, TyVar};
use super::unify::Unifier;
use crate::span::Span;
use std::collections::{HashMap, HashSet};

// ─────────────────────────────────────────────────────────────────────────────
// Free-variable collection
// ─────────────────────────────────────────────────────────────────────────────

/// All three variable kinds, collected in one traversal.
pub struct FreeVars {
    pub tys: HashSet<TyVar>,
    pub modes: HashSet<ModeVar>,
    pub rows: HashSet<RowVar>,
}

impl FreeVars {
    pub fn new() -> Self {
        FreeVars {
            tys: HashSet::new(),
            modes: HashSet::new(),
            rows: HashSet::new(),
        }
    }
}

pub fn free_ty(u: &mut Unifier, ty: &Ty, out: &mut FreeVars) {
    match u.resolve_ty(ty) {
        Ty::Var(v) => {
            out.tys.insert(v);
        }
        Ty::List(a) | Ty::Map(a) | Ty::Handle(a) => free_ty(u, &a, out),
        Ty::Record(r) => free_row(u, &r, out),
        Ty::Thunk(b) => free_comp(u, &b, out),
        _ => {}
    }
}

fn free_row(u: &mut Unifier, row: &Row, out: &mut FreeVars) {
    match u.resolve_row(row) {
        Row::Empty => {}
        Row::Var(v) => {
            out.rows.insert(v);
        }
        Row::Extend(_, ty, rest) => {
            free_ty(u, &ty, out);
            free_row(u, &rest, out);
        }
    }
}

fn free_comp(u: &mut Unifier, cty: &CompTy, out: &mut FreeVars) {
    match u.resolve_comp_ty(cty) {
        CompTy::Var(_) => {}
        CompTy::Return(spec, a) => {
            free_mode(u, &spec.input, out);
            free_mode(u, &spec.output, out);
            free_ty(u, &a, out);
        }
        CompTy::Fun(a, b) => {
            free_ty(u, &a, out);
            free_comp(u, &b, out);
        }
    }
}

fn free_mode(u: &mut Unifier, mode: &PipeMode, out: &mut FreeVars) {
    match u.resolve_mode(mode) {
        PipeMode::Var(v) => {
            out.modes.insert(v);
        }
        PipeMode::Values(a) => free_ty(u, &a, out),
        _ => {}
    }
}

/// Collect free variables across all schemes in the environment.
pub fn env_free_vars(u: &mut Unifier, env: &TyEnv) -> FreeVars {
    let mut out = FreeVars::new();
    for s in env.all_schemes() {
        if let Some(cached) = &s.cached_fv {
            out.tys.extend(&cached.ty_fv);
            out.modes.extend(&cached.mode_fv);
            out.rows.extend(&cached.row_fv);
        } else {
            let mut fvs = FreeVars::new();
            free_ty(u, &s.ty, &mut fvs);
            for v in &s.ty_vars {
                fvs.tys.remove(v);
            }
            for v in &s.mode_vars {
                fvs.modes.remove(v);
            }
            for v in &s.row_vars {
                fvs.rows.remove(v);
            }
            out.tys.extend(fvs.tys);
            out.modes.extend(fvs.modes);
            out.rows.extend(fvs.rows);
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Typing environment
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct TyEnv {
    scopes: Vec<HashMap<String, Scheme>>,
}

impl Default for TyEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl TyEnv {
    pub fn new() -> Self {
        TyEnv {
            scopes: vec![HashMap::new()],
        }
    }

    pub fn lookup(&self, name: &str) -> Option<&Scheme> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    pub fn push(&mut self) {
        self.scopes.push(HashMap::new());
    }
    pub fn pop(&mut self) {
        self.scopes.pop();
    }

    pub fn bind(&mut self, name: String, scheme: Scheme) {
        self.scopes.last_mut().unwrap().insert(name, scheme);
    }

    pub fn all_schemes(&self) -> impl Iterator<Item = &Scheme> {
        self.scopes.iter().flat_map(|s| s.values())
    }

    pub fn all_named_schemes(&self) -> impl Iterator<Item = (String, Scheme)> + '_ {
        self.scopes
            .iter()
            .flat_map(|s| s.iter().map(|(k, v)| (k.clone(), v.clone())))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Inference context
// ─────────────────────────────────────────────────────────────────────────────

pub struct InferCtx {
    pub unifier: Unifier,
    pub errors: Vec<TypeError>,
    pub pos: Option<Span>,
}

impl Default for InferCtx {
    fn default() -> Self {
        Self::new()
    }
}

impl InferCtx {
    pub fn new() -> Self {
        InferCtx {
            unifier: Unifier::new(),
            errors: Vec::new(),
            pos: None,
        }
    }

    pub fn error(&mut self, msg: String) {
        self.emit_kind(TypeErrorKind::AdHoc { message: msg }, None);
    }

    pub fn error_hint(&mut self, msg: String, hint: &str) {
        self.emit_kind(TypeErrorKind::AdHoc { message: msg }, Some(hint));
    }

    /// Push a type error from the unifier or inferencer.
    pub fn emit_kind(&mut self, kind: TypeErrorKind, hint: Option<&str>) {
        self.errors.push(TypeError {
            pos: self.pos,
            kind,
            hint: hint.map(|s| s.to_string()),
        });
    }

    pub fn unify_ty(&mut self, a: &Ty, b: &Ty) {
        if let Err(kind) = self.unifier.unify_ty(a, b) {
            self.emit_kind(kind, None);
        }
    }

    pub fn unify_comp_ty(&mut self, a: &CompTy, b: &CompTy) {
        if let Err(kind) = self.unifier.unify_comp_ty(a, b) {
            self.emit_kind(kind, None);
        }
    }

    pub fn unify_mode(&mut self, a: &PipeMode, b: &PipeMode) {
        if let Err(kind) = self.unifier.unify_mode(a, b) {
            self.emit_kind(kind, None);
        }
    }

    pub fn unify_ty_hint(&mut self, a: &Ty, b: &Ty, hint: &str) {
        if let Err(kind) = self.unifier.unify_ty(a, b) {
            self.emit_kind(kind, Some(hint));
        }
    }

    pub fn unify_comp_ty_hint(&mut self, a: &CompTy, b: &CompTy, hint: &str) {
        if let Err(kind) = self.unifier.unify_comp_ty(a, b) {
            self.emit_kind(kind, Some(hint));
        }
    }
}
