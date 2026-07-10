//! Hindley-Milner type inference for ral.
//!
//! Types sit at the CBPV IR level — on Val and Comp after elaboration.
//! Value types (A) describe data; computation types (B) describe effectful
//! computations with pipeline modes.  Polymorphism by let-generalisation.
//!
//! The two sorts:
//!
//!   `Value ::= Unit | Bool | Int | Float | String | [Value] | [String:Value]`
//!   `       | {l₁:V₁, …, lₙ:Vₙ | row}`   -- record (row-polymorphic)
//!   `       | {Comp} | Handle | TypeVar`
//!   `Comp ::= F[I,O] Value | Value → Comp | CompVar`
//!   `I,O ::= ∅ | Bytes | IOVar`
//!
//! Generalisation happens at Bind (let) nodes, and also at `LetRec`: each
//! recursive binding is typed monomorphically against itself first
//! (`infer_letrec_betas`), then generalised once the group's mono
//! self-bindings are dropped from scope, so mutually recursive bindings
//! still end up polymorphic at their use sites.

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

// Public re-exports: preserve the existing `typecheck::Ty`, `typecheck::CompTy`,
// etc. paths consumed by main.rs and the test suite.
pub use self::builtins::{builtin_arity, builtin_type_hint};
pub use self::env::{InferCtx, TyEnv};
pub use self::error::{CompDiff, Reason, TypeError, TypeErrorKind};
pub use self::fmt::{FmtCtx, fmt_mode, fmt_mode_ctx, fmt_scheme, fmt_ty, fmt_ty_ctx};
pub use self::scheme::{CachedFreeVars, Scheme};
pub use self::ty::{CompTy, CompTyVar, ModeVar, PipeMode, PipeSpec, Row, RowVar, Ty, TyVar};
pub use self::unify::Unifier;

use self::generalize::generalize;
use crate::ir::{Comp, CompKind, IrPattern};

/// The live session as the checker sees it:
///
/// every top-level name with
/// the scheme on its runtime binding (`None` for a name installed by an
/// unchecked path — a `source`d file, a plugin), plus the persistent
/// alias arms' schemes off the handler stack.
///
/// This is the single seed of a turn's check.  A name with a scheme is
/// bound to it; a name without one is a bare name that elaborates as a
/// variable and infers at a fresh type variable per use site.
#[derive(Debug, Clone, Default)]
pub struct SessionSchemes {
    pub bindings: Vec<(String, Option<Scheme>)>,
    pub aliases: Vec<(String, Scheme)>,
}

impl SessionSchemes {
    /// Seed of checked bindings alone — the baked prelude list, for
    /// callers with no live shell (`--check`, batch scripts, tests).
    pub fn from_schemes(schemes: &[(String, Scheme)]) -> Self {
        Self {
            bindings: schemes
                .iter()
                .map(|(name, scheme)| (name.clone(), Some(scheme.clone())))
                .collect(),
            aliases: Vec::new(),
        }
    }
}

/// Seed the typing environment from a live session.  Checked bindings
/// bind their scheme; an unchecked entry (`None`) is skipped, leaving
/// the name to elaborate as a bare variable.  Alias arms bind as
/// removable handler frames so a turn-level `unalias` unbinds them
/// statically.  Builtins own their names and are never overwritten.
fn seed_env(env: &mut TyEnv, schemes: SessionSchemes) {
    for (name, scheme) in schemes.bindings {
        if crate::builtins::is_builtin(&name) {
            continue;
        }
        if let Some(scheme) = scheme {
            env.bind(name, scheme);
        }
    }
    for (name, scheme) in schemes.aliases {
        if crate::builtins::is_builtin(&name) {
            continue;
        }
        env.bind_handler(name, scheme, true);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Public entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Type-check `comp`, seeding the type environment from the live session.
///
/// The scheme verdict rides inside the returned comp: the annotation pass
/// writes each top-level bind's generalised scheme onto its `Bind` node.
/// Only a closed scheme leaves the checker — each turn's unifier dies
/// with the turn and its variable ids restart at zero, so an open scheme
/// from turn *N* would alias turn *N+1*'s fresh variables (see the ADR
/// session-scheme-continuity).
///
/// # Errors
/// Returns `Err` with every diagnostic the inference pass collected —
/// unbound identifiers, type/row/mode unification mismatches, arity and
/// application errors — whenever that list is non-empty.
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

/// Read the (name, scheme) pairs off an annotated comp's top-level
/// `Bind` nodes — the one harvest behind both the build-time prelude
/// bake and a turn's installs.
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
/// harvested off its top-level `Bind` nodes.
///
/// Called by `ral/build.rs` at build time and by tests at runtime.
/// Callers serialise the *annotated* prelude, so the comp blob and the
/// schemes blob come out of one checked pass; evaluating the annotated
/// prelude installs each binding's scheme next to its value.
///
/// The pass runs through the same [`typecheck`] entry the runtime uses,
/// seeded with an empty session — builtins are resolved dynamically during
/// inference, and no prior bindings exist at bake time.  A type error is
/// fatal: the build script panics with the formatted errors.
///
/// # Panics
/// Panics if the prelude fails to type-check, reporting the errors.
pub fn bake_prelude(comp: &Comp) -> (Comp, Vec<(String, Scheme)>) {
    let annotated = match typecheck(comp, SessionSchemes::default()) {
        Ok(a) => a,
        Err(errs) => {
            let msgs: Vec<String> = errs.iter().map(ToString::to_string).collect();
            panic!("prelude type errors:\n{}", msgs.join("\n"));
        }
    };
    let schemes = harvest_schemes(&annotated)
        .into_iter()
        .filter(|(name, _)| !crate::builtins::is_builtin(name))
        .collect();
    (annotated, schemes)
}

/// The scheme stored on a persistent alias frame at install
/// (`Shell::install_alias`):
///
/// the arm body for head `head` inferred under
/// the runtime handler calling convention (a lambda arm receives the argv
/// list), seeded from the live session and closed against its own unifier
/// so later turns' checks can be seeded with it.
///
/// The arm's `PipeSpec` is pinned to the head's known spec — the session
/// scheme for `head` when one exists, otherwise the external default
/// `F[μ, Bytes]` — leaving the arm's value type free.  A mode-changing arm
/// (a value-output body under a byte-output head) is the lone rejected
/// failure, returned as a [`crate::mode::ModeMismatch`]; all other arm
/// inference errors keep today's behaviour — an `alias` statement's arm was
/// already checked by its turn, and rc/plugin arms surface their failures
/// at use.
///
/// # Errors
/// Returns `Err` if pinning the arm to the head's spec finds a mode clash
/// — a value-output body under a byte-output head.
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

/// The scheme stored for a value binding installed as a lexical scope binding
/// (`Shell::bind_value`, the rc `bindings:` path):
///
/// the value's type
/// inferred under the ordinary value/function-application convention — a
/// lambda is a function `Fun(param, body)` whose parameter binds an
/// independent value type, not an argv-forced arm — seeded from the live
/// session and closed against its own unifier so later turns' checks can
/// be seeded with it.  A lexical binding is not pinned to a head, so this
/// returns the scheme directly.
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
