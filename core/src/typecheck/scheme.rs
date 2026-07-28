//! Type schemes (`forall alpha. A`): a type under universal quantifiers.
//!
//! `generalize.rs` builds these at `let` bindings and instantiates them with
//! fresh unification variables at each use site, giving let-polymorphism.

use super::ty::{CompTy, CompTyVar, ModeVar, RowVar, Ty, TyVar};
use std::collections::BTreeSet;

/// The free variables a scheme did *not* quantify, being already free in the
/// environment.  `env_free_vars` reads these rather than re-walking the type;
/// every set is empty for a fully-generalised scheme.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CachedFreeVars {
    pub ty_fv: BTreeSet<TyVar>,
    #[serde(default)]
    pub comp_fv: BTreeSet<CompTyVar>,
    pub mode_fv: BTreeSet<ModeVar>,
    pub row_fv: BTreeSet<RowVar>,
}

/// A polymorphic type scheme: `forall alpha_1 ... alpha_n, gamma_1 ... gamma_l, rho_1 ... rho_k, mu_1 ... mu_m. A`.
///
/// Quantifies value types, computation types, rows, and pipeline modes at
/// once.  A variable caught in a cycle cannot be a plain quantifier, so
/// `comp_ty_bindings` and `ty_bindings` snapshot it as `(original root id,
/// applied binding)`; instantiation mints a fresh id per entry and re-binds
/// it, so two instantiations never share the cycle's union-find slot.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Scheme {
    pub ty_vars: Vec<TyVar>,
    #[serde(default)]
    pub comp_ty_vars: Vec<CompTyVar>,
    pub mode_vars: Vec<ModeVar>,
    pub row_vars: Vec<RowVar>,
    pub ty: Ty,
    #[serde(default)]
    pub comp_ty_bindings: Vec<(u32, CompTy)>,
    #[serde(default)]
    pub ty_bindings: Vec<(u32, Ty)>,
    /// `None` while the residuals can still move: a monomorphic scheme's free
    /// variables shift as unification proceeds, so only `generalize` and the
    /// closed builtin schemes may fill this in.
    pub cached_fv: Option<CachedFreeVars>,
}

impl Scheme {
    /// A scheme with nothing quantified.
    pub fn mono(ty: Ty) -> Self {
        Self {
            ty_vars: vec![],
            comp_ty_vars: vec![],
            mode_vars: vec![],
            row_vars: vec![],
            ty,
            comp_ty_bindings: vec![],
            ty_bindings: vec![],
            cached_fv: None,
        }
    }
    /// True when instantiation has work to do.  Cyclic bindings count: a
    /// scheme with no quantifiers but a captured cycle still needs fresh roots.
    pub fn is_poly(&self) -> bool {
        !self.ty_vars.is_empty()
            || !self.comp_ty_vars.is_empty()
            || !self.mode_vars.is_empty()
            || !self.row_vars.is_empty()
            || !self.comp_ty_bindings.is_empty()
            || !self.ty_bindings.is_empty()
    }
}
