//! Hindley-Milner type inference for ral.
//!
//! Types sit at the CBPV IR level — on Val and Comp after elaboration.
//! Value types (A) describe data; computation types (B) describe effectful
//! computations with pipeline modes.  Polymorphism by let-generalisation.
//!
//! The two sorts:
//!
//!   A ::= Unit | Bool | Int | Float | String | [A] | [String:A]
//!       | {l₁:A₁, …, lₙ:Aₙ | ρ}   -- record (row-polymorphic)
//!       | {B} | Handle | α
//!   B ::= F[I,O] A | A → B | β
//!   I,O ::= ∅ | Bytes | Values(A) | μ
//!
//! Generalisation happens at Bind (let) nodes.  Recursive bindings (LetRec,
//! Rec) are given monomorphic types to prevent unsound generalisation.

mod builtins;
mod env;
mod fmt;
mod generalize;
mod infer;
mod scheme;
mod ty;
mod unify;

// Public re-exports: preserve the existing `typecheck::Ty`, `typecheck::CompTy`,
// etc. paths consumed by main.rs and the test suite.
pub use self::builtins::builtin_type_hint;
pub use self::env::{InferCtx, TyEnv};
pub use self::fmt::{fmt_comp_ty, fmt_mode, fmt_scheme, fmt_ty};
pub use self::scheme::{CompDiff, Scheme, TypeError, TypeErrorKind};
pub use self::ty::{CompTy, CompTyVar, ModeVar, PipeMode, PipeSpec, Row, RowVar, Ty, TyVar};
pub use self::unify::Unifier;

use crate::ir::Comp;

/// Compute the prelude type schemes from the given prelude IR.
///
/// Called by `ral/build.rs` at build time to bake the schemes, and by
/// tests at runtime.
pub fn bake_prelude_schemes(comp: &Comp) -> Vec<(String, Scheme)> {
    let mut ctx = InferCtx::new();
    let mut env = TyEnv::new();
    seed_builtins(&mut ctx.unifier, &mut env);
    infer::infer_comp(&mut ctx, &mut env, comp);
    env.all_named_schemes().collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Public entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Type-check `comp`, seeding the prelude type environment from `prelude_schemes`.
pub fn typecheck(comp: &Comp, prelude_schemes: &[(String, Scheme)]) -> Vec<TypeError> {
    let mut ctx = InferCtx::new();
    let mut env = TyEnv::new();

    for (name, scheme) in prelude_schemes {
        env.bind(name.clone(), scheme.clone());
    }

    infer::infer_comp(&mut ctx, &mut env, comp);
    ctx.errors
}

/// Seed the typing environment with builtin names that may appear as
/// variables (e.g. `$length` or in value-head position after prelude wraps them).
///
/// `builtin_scheme` allocates fresh unifier vars directly, so the returned
/// scheme is already properly registered and can be stored as-is.
fn seed_builtins(u: &mut Unifier, env: &mut TyEnv) {
    for name in crate::builtins::builtin_names() {
        if let Some(scheme) = builtins::builtin_scheme(name, u) {
            env.bind((*name).to_string(), scheme);
        }
    }
}
