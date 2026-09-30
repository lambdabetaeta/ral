//! Type schemes (`forall alpha. A`): a type under universal quantifiers.
//!
//! `generalize.rs` builds these at `let` bindings and instantiates them with
//! fresh unification variables at each use site, giving let-polymorphism.

use super::kind::Kind;
use super::ty::{CompTy, CompTyVar, RowVar, Ty, TyVar};
use std::collections::{BTreeMap, BTreeSet};

/// The free variables a scheme did *not* quantify, being already free in the
/// environment.  `env_free_vars` reads these rather than re-walking the type;
/// every set is empty for a fully-generalised scheme.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[allow(clippy::struct_field_names)] // one set per variable sort; the sort is the prefix.
pub struct CachedFreeVars {
    pub(crate) ty_fv: BTreeSet<TyVar>,
    #[serde(default)]
    pub(crate) comp_fv: BTreeSet<CompTyVar>,
    pub(crate) row_fv: BTreeSet<RowVar>,
}

/// The weak variables a scheme mentions and did not quantify: one type per
/// unit, so a later unit re-seeds each as a fresh weak variable
/// (`generalize::reseed_weak`) instead of reading a foreign id.  A type
/// variable keeps its kind, and a row variable its deep bit.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WeakVars {
    pub(crate) tys: BTreeMap<TyVar, Kind>,
    pub(crate) comps: BTreeSet<CompTyVar>,
    pub(crate) rows: BTreeMap<RowVar, bool>,
}

impl WeakVars {
    pub(crate) fn is_empty(&self) -> bool {
        self.tys.is_empty() && self.comps.is_empty() && self.rows.is_empty()
    }
}

/// A polymorphic type scheme: `forall (alpha_1:k_1) ... (alpha_n:k_n), gamma_1 ... gamma_l, rho_1 ... rho_k. A`.
///
/// Quantifies value types, computation types and rows at once.
///
/// A variable caught in a cycle cannot be a plain quantifier, so
/// `comp_ty_bindings` and `ty_bindings` snapshot it as `(original root id,
/// applied binding)`; instantiation mints a fresh id per entry and re-binds
/// it, so two instantiations never share the cycle's union-find slot.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Scheme {
    pub(crate) ty_vars: Vec<(TyVar, Kind)>,
    #[serde(default)]
    pub(crate) comp_ty_vars: Vec<CompTyVar>,
    pub(crate) row_vars: Vec<(RowVar, bool)>,
    pub(crate) ty: Ty,
    #[serde(default)]
    pub(crate) comp_ty_bindings: Vec<(u32, CompTy)>,
    #[serde(default)]
    pub(crate) ty_bindings: Vec<(u32, Ty)>,
    /// `None` while the residuals can still move: a monomorphic scheme's free
    /// variables shift as unification proceeds, so only `generalize` and the
    /// closed builtin schemes may fill this in.
    pub(crate) cached_fv: Option<CachedFreeVars>,
    /// Shared with every other use in the unit, never quantified.
    #[serde(default)]
    pub(crate) weak: WeakVars,
}

impl Scheme {
    /// A scheme with nothing quantified.
    pub(crate) fn mono(ty: Ty) -> Self {
        Self {
            ty_vars: vec![],
            comp_ty_vars: vec![],
            row_vars: vec![],
            ty,
            comp_ty_bindings: vec![],
            ty_bindings: vec![],
            cached_fv: None,
            weak: WeakVars::default(),
        }
    }
    /// True when instantiation has work to do.  Cyclic bindings count: a
    /// scheme with no quantifiers but a captured cycle still needs fresh roots.
    pub(crate) fn is_poly(&self) -> bool {
        !self.ty_vars.is_empty()
            || !self.comp_ty_vars.is_empty()
            || !self.row_vars.is_empty()
            || !self.comp_ty_bindings.is_empty()
            || !self.ty_bindings.is_empty()
    }
}
