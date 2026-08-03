//! Hindley-Milner inference over the CBPV IR: value types on `Val`,
//! computation types on `Comp`, polymorphism by let-generalisation.
//!
//! Seeding and entry points live here; the inference rules live in `infer`.

mod annotate;
pub mod builtins;
mod env;
mod error;
mod explain;
mod fmt;
mod generalize;
pub(crate) mod infer;
mod scheme;
mod scope;
mod ty;
mod unify;

pub use self::builtins::builtin_type_hint;
pub use self::env::{InferCtx, TyEnv};
pub use self::error::{CompDiff, Reason, TypeError, TypeErrorKind};
pub use self::fmt::{FmtCtx, fmt_mode, fmt_mode_ctx, fmt_scheme, fmt_ty, fmt_ty_ctx};
pub use self::scheme::{CachedFreeVars, Scheme};
pub use self::ty::{
    ByteMode, CompTy, CompTyVar, ModeVar, PipeMode, PipeSpec, Row, RowVar, Ty, TyVar,
};
pub use self::unify::Unifier;

use self::generalize::generalize;
use crate::ir::{Comp, CompKind, IrPattern};

/// The seed of a run's check, read off the live session by
/// `Shell::session_schemes`.
///
/// A binding with no scheme came from an unchecked path — a `source`d file, a
/// plugin — and infers at a fresh variable per use site.
#[derive(Debug, Clone)]
pub struct SessionSchemes {
    pub bindings: Vec<(String, Option<Scheme>)>,
    pub aliases: Vec<(String, Scheme)>,
    pub builtins: crate::types::BuiltinTable,
}

impl Default for SessionSchemes {
    /// Core builtins alone, no host dressing: `bake_prelude` at build time and
    /// the structural-frontend tests, which check with no live shell.
    fn default() -> Self {
        Self {
            bindings: Vec::new(),
            aliases: Vec::new(),
            builtins: crate::builtins::core_builtin_table(),
        }
    }
}

impl SessionSchemes {
    /// Seed from a fixed scheme list — the baked prelude — against a host
    /// surface's table, for callers with no live shell: `--check`, batch, tests.
    pub fn from_schemes(
        schemes: &[(String, Scheme)],
        builtins: crate::types::BuiltinTable,
    ) -> Self {
        Self {
            bindings: schemes
                .iter()
                .map(|(name, scheme)| (name.clone(), Some(scheme.clone())))
                .collect(),
            aliases: Vec::new(),
            builtins,
        }
    }
}

/// No filter: natives never arrive in this harvest (the bindings walk is
/// user scopes only), and a binding sharing a native's name shadows it, as
/// at runtime.
fn seed_env(env: &mut TyEnv, schemes: SessionSchemes) {
    env.builtins = schemes.builtins;
    for (name, scheme) in schemes.bindings {
        if let Some(scheme) = scheme {
            env.bind(name, scheme);
        }
    }
    for (name, scheme) in schemes.aliases {
        env.bind_handler(name, scheme, true);
    }
}

/// Type-check `comp`, seeding from the live session.
///
/// The verdict rides back on the returned comp: `annotate` writes each
/// top-level bind's generalised scheme onto its `Bind` node.  Only closed
/// schemes leave — a run's unifier dies with the run and its variable ids
/// restart at zero, so an open scheme from run *N* would alias run *N+1*'s
/// fresh variables.
///
/// # Errors
/// Every diagnostic the inference pass collected, whenever that list is non-empty.
pub fn typecheck(comp: &Comp, schemes: SessionSchemes) -> Result<Comp, Vec<TypeError>> {
    let mut ctx = InferCtx::new();
    let mut env = TyEnv::new();
    seed_env(&mut env, schemes);

    infer::infer_comp(&mut ctx, &mut env, comp);
    if ctx.errors.is_empty() {
        Ok(annotate::annotate(comp, &mut ctx, true))
    } else {
        Err(ctx.errors)
    }
}

/// The (name, scheme) pairs on an *annotated* comp's top-level `Bind` nodes.
fn harvest_schemes(comp: &Comp) -> Vec<(String, Scheme)> {
    let mut out = Vec::new();
    harvest_into(comp, &mut out);
    out
}

fn harvest_into(comp: &Comp, out: &mut Vec<(String, Scheme)>) {
    match &comp.item {
        CompKind::Seq(parts) => {
            for part in parts {
                harvest_into(part, out);
            }
        }
        CompKind::Bind {
            pattern: IrPattern::Name(name),
            rest,
            scheme: Some(scheme),
            ..
        } => {
            out.push((name.clone(), (**scheme).clone()));
            harvest_into(rest, out);
        }
        CompKind::Bind { rest, .. } => harvest_into(rest, out),
        _ => {}
    }
}

/// Type-check the prelude IR, returning the annotated comp and the schemes
/// on its top-level `Bind` nodes.
///
/// Callers — `boot::bake_prelude_to_out_dir`, from each host's build script
/// — serialise the *annotated* prelude, so the comp blob and the scheme
/// blob come out of one checked pass and evaluating the prelude installs
/// each binding's scheme beside its value.  A prelude binding named after a
/// native seeds and shadows like any other.
///
/// # Panics
/// If the prelude fails to type-check, reporting the errors.
pub fn bake_prelude(comp: &Comp) -> (Comp, Vec<(String, Scheme)>) {
    let seed = SessionSchemes::default();
    let annotated = match typecheck(comp, seed) {
        Ok(a) => a,
        Err(errs) => {
            let msgs: Vec<String> = errs.iter().map(ToString::to_string).collect();
            panic!("prelude type errors:\n{}", msgs.join("\n"));
        }
    };
    let schemes = harvest_schemes(&annotated);
    (annotated, schemes)
}

/// The scheme for a handler arm, computed by `HandlerEntry::vet` at install
/// and persisted only on alias frames, which outlive their run.
///
/// The arm is inferred under the runtime handler calling convention — a
/// lambda arm receives the argv list — and closed against its own unifier
/// so a later run's check can be seeded with it.  Pinning constrains only
/// the arm's `PipeSpec`, leaving its value type free: to the head's handler
/// scheme when one is in scope, otherwise to a fresh `F[μ, ν]` the arm
/// itself pins down.
///
/// # Errors
/// The lone rejection: a ground mode clash, a value-output body under a
/// byte-output head.
pub fn alias_arm_scheme(
    head: &str,
    param: &crate::ir::IrPattern,
    body: &Comp,
    schemes: SessionSchemes,
) -> Result<Scheme, crate::mode::ModeMismatch> {
    let mut ctx = InferCtx::new();
    let mut env = TyEnv::new();
    seed_env(&mut env, schemes);
    let mut inferencer = infer::Inferencer {
        ctx: &mut ctx,
        env: &mut env,
    };
    let cty = inferencer.infer_alias_arm(Some(param), body);
    inferencer.pin_arm_to_head(head, &cty)?;
    let thunk_ty = Ty::Thunk(Box::new(cty));
    let scheme = generalize(&mut ctx.unifier, &TyEnv::new(), &thunk_ty);
    self::generalize::debug_assert_scheme_closed(
        &mut ctx.unifier,
        &scheme,
        "alias-arm scheme must leave no variable free",
    );
    Ok(scheme)
}

/// The scheme for a value binding (`Shell::bind_value`, `Shell::register_hook`),
/// inferred under the ordinary value/function-application convention.
///
/// A lambda is a `Fun(param, body)` whose parameter binds an independent
/// value type, not the argv list an alias arm is forced onto.  Closed
/// against its own unifier; with no head to pin to, the scheme comes back
/// directly.
pub fn binding_value_scheme(
    param: Option<&crate::ir::IrPattern>,
    body: &Comp,
    schemes: SessionSchemes,
) -> Scheme {
    let mut ctx = InferCtx::new();
    let mut env = TyEnv::new();
    seed_env(&mut env, schemes);
    let mut inferencer = infer::Inferencer {
        ctx: &mut ctx,
        env: &mut env,
    };
    let cty = inferencer.infer_binding_value(param, body);
    let thunk_ty = Ty::Thunk(Box::new(cty));
    let scheme = generalize(&mut ctx.unifier, &TyEnv::new(), &thunk_ty);
    self::generalize::debug_assert_scheme_closed(
        &mut ctx.unifier,
        &scheme,
        "binding-value scheme must leave no variable free",
    );
    scheme
}
