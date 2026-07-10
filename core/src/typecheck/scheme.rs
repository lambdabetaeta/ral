//! Type schemes (`forall alpha. A`).
//!
//! A `Scheme` is a type with universally quantified variables — the result
//! of generalisation at `let` bindings.  Instantiation replaces quantified
//! variables with fresh unification variables at each use site, giving
//! let-polymorphism.

use super::ty::{CompTy, CompTyVar, ModeVar, RowVar, Ty, TyVar};
use std::collections::BTreeSet;

// ─────────────────────────────────────────────────────────────────────────────
// Type scheme:  ∀α₁…αₙ ∀γ₁…γₗ ∀ρ₁…ρₖ ∀μ₁…μₘ. A
// ─────────────────────────────────────────────────────────────────────────────

/// Cached residual free variables for a scheme — those free in the scheme's
/// type that were NOT quantified because they appeared in the environment at
/// generalisation time.
///
/// For fully-generalised (top-level) schemes all three
/// sets are empty.
///
/// Stored on generalised schemes so that `env_free_vars` can skip a full
/// type-tree traversal and read the cached sets directly.
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
/// Quantifies over four variable kinds simultaneously: value types,
/// computation types, row types, and pipeline modes.  `ty` is the body of
/// the scheme — the type under the quantifiers.
///
/// Recursive types — both computation and value — are captured by
/// `comp_ty_bindings` and `ty_bindings`: snapshots of `(old_root,
/// applied_binding)` pairs for every var that is part of a cycle in the
/// scheme's body.  At instantiation time each entry is given a fresh var
/// id and re-bound to the binding with substitutions applied, so two
/// instantiations of the same scheme do not share a union-find slot for
/// the cycle root.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Scheme {
    pub ty_vars: Vec<TyVar>,
    /// Quantified non-cyclic comp-type variables.  `instantiate` mints
    /// fresh ids for each entry so polymorphic schemes whose body
    /// contains a free comp var (e.g. `Thunk(γ)` for an unconstrained
    /// γ) do not share that var across use sites.
    #[serde(default)]
    pub comp_ty_vars: Vec<CompTyVar>,
    pub mode_vars: Vec<ModeVar>,
    pub row_vars: Vec<RowVar>,
    pub ty: Ty,
    /// Snapshotted cyclic comp-var bindings (key: original root id).
    /// Empty for non-recursive schemes.  Generalisation populates this
    /// from the unifier's union-find; instantiation re-binds fresh ids
    /// to the substituted bindings.
    #[serde(default)]
    pub comp_ty_bindings: Vec<(u32, CompTy)>,
    /// Snapshotted cyclic ty-var bindings (key: original root id).
    /// Mirror of `comp_ty_bindings` for value-type cycles such as the
    /// streaming-consumer α := Variant {`more {head, tail: Thunk(α)},
    /// `done | ρ}.
    #[serde(default)]
    pub ty_bindings: Vec<(u32, Ty)>,
    /// Pre-computed residual free variables.  `None` for monomorphic schemes
    /// whose free variables change as unification proceeds.  `Some` for
    /// schemes produced by `generalize()` or for fully-closed builtins.
    pub cached_fv: Option<CachedFreeVars>,
}

impl Scheme {
    /// A monomorphic scheme: no quantified variables.
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
    /// True when the scheme quantifies over at least one variable.
    pub fn is_poly(&self) -> bool {
        !self.ty_vars.is_empty()
            || !self.comp_ty_vars.is_empty()
            || !self.mode_vars.is_empty()
            || !self.row_vars.is_empty()
            || !self.comp_ty_bindings.is_empty()
            || !self.ty_bindings.is_empty()
    }
}
