//! Hindley-Milner inference over the CBPV IR: value types on `Val`,
//! computation types on `Comp`, polymorphism by let-generalisation.
//!
//! Seeding and entry points live here; the inference rules live in `infer`.

mod annotate;
pub mod builtins;
mod capture;
pub mod contract;
mod env;
mod error;
mod explain;
mod fmt;
mod generalize;
mod index;
pub(crate) mod infer;
mod kind;
mod scheme;
mod scope;
mod ty;
mod unify;

pub use self::builtins::builtin_type_hint;
pub use self::contract::{Form, Table};
pub use self::env::{InferCtx, TyEnv};
pub use self::error::{CycleVia, KindFound, Reason, Standing, TypeError, TypeErrorKind};
pub(crate) use self::explain::NO_STATUS_REGISTER;
pub use self::fmt::{FmtCtx, fmt_comp_ty_ctx, fmt_scheme, fmt_ty, fmt_ty_ctx};
pub use self::kind::Kind;
pub use self::scheme::Scheme;
#[cfg(feature = "test-util")]
pub(crate) use self::scheme::WeakVars;
pub use self::ty::{CompTy, CompTyVar, Grade, GradeVar, Label, Row, RowVar, Ty, TyVar};
pub use self::unify::Unifier;

#[cfg(any(test, feature = "test-util"))]
pub(crate) use self::generalize::has_result_only_var;
use self::generalize::{FreeVars, generalize, settle_weak};
pub(crate) use self::generalize::{instantiate, reseed_weak};
use crate::ir::{Comp, Phrase, Toplevel};
use std::sync::Arc;

/// What a form holds its programs' own return value to: the declared
/// [`Table`] whose closed keyset the returned row is checked against.
///
/// An rc file's top-level keys, a plugin manifest's fields, a capability
/// profile's dimensions.  `&'static`, so a host crate with a table of its own
/// passes it here.
pub type ReturnContract = &'static Table;

/// The seed of a run's check, read off the live session by
/// `Shell::session_schemes`.
///
/// A binding with no scheme came from `Shell::set_var` — a host seed var, an
/// rc's `env:`/`prompt:` key — and is bound at one fresh monomorphic type
/// variable: generalising it to `∀a. a` would let it stand in for any type at
/// all, unsoundly.
#[derive(Debug, Clone)]
pub struct SessionSchemes {
    pub(crate) bindings: Vec<(String, Option<Arc<Scheme>>)>,
    pub(crate) aliases: Vec<(String, Arc<Scheme>)>,
    pub(crate) builtins: crate::types::BuiltinTable,
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
                .map(|(name, scheme)| (name.clone(), Some(Arc::new(scheme.clone()))))
                .collect(),
            aliases: Vec::new(),
            builtins,
        }
    }
}

/// No filter: natives never arrive in this harvest (the bindings walk is
/// user scopes only), and a binding sharing a native's name shadows it, as
/// at runtime.
///
/// The manifest's argv half seeds handlers, because that is what a base frame
/// is: an arm on a name, reached in command position, taking an argv.  It goes
/// in first, so a user arm installed over one shadows it here as it does at
/// runtime — and it is not removable by `unalias`, there being no frame under
/// it to fall back to.
fn seed_env(
    env: &mut TyEnv,
    schemes: SessionSchemes,
    u: &mut Unifier,
) -> Vec<(String, Arc<Scheme>)> {
    env.builtins = schemes.builtins;
    let frames: Vec<(String, Scheme)> = env
        .builtins
        .base_frames()
        .map(|entry| (entry.name.as_ref().to_string(), (entry.type_rule)(u)))
        .collect();
    for (name, scheme) in frames {
        env.bind_handler(name, scheme, false);
    }
    let mut residuals = Vec::new();
    for (name, scheme) in schemes.bindings {
        let scheme = match scheme {
            Some(scheme) => reseed_weak(u, scheme),
            None => Arc::new(Scheme::mono(u.fresh_ty())),
        };
        // A block's results are admitted by the sites in its own IR; a data
        // binding's value is here, to be admitted against this unit's uses.
        if !scheme.weak.is_empty() && !matches!(scheme.ty, Ty::Thunk(_)) {
            residuals.push((name.clone(), Arc::clone(&scheme)));
        }
        env.bind(name, scheme);
    }
    for (name, scheme) in schemes.aliases {
        env.bind_handler(name, reseed_weak(u, scheme), true);
    }
    residuals
}

/// Build a fresh `(InferCtx, TyEnv)` pair seeded from `schemes` — the common
/// core of every inference session, `typecheck` included.
fn seeded_session(schemes: SessionSchemes) -> (InferCtx, TyEnv) {
    let mut ctx = InferCtx::new();
    let mut env = TyEnv::new();
    ctx.residuals = seed_env(&mut env, schemes, &mut ctx.unifier);
    (ctx, env)
}

/// Run `f` inside a fresh one-shot inference session over `schemes` — closed
/// against its own unifier, and dropped when `f` returns so its variable ids
/// cannot alias a later run's.
fn one_shot_inference<R>(
    schemes: SessionSchemes,
    f: impl FnOnce(&mut infer::Inferencer) -> R,
) -> R {
    let (mut ctx, mut env) = seeded_session(schemes);
    let mut inferencer = infer::Inferencer {
        ctx: &mut ctx,
        env: &mut env,
    };
    f(&mut inferencer)
}

/// Close `cty` into a session-independent `Thunk` scheme: solve the session,
/// generalize, and assert nothing escaped. Shared by [`alias_arm_scheme`] and
/// [`binding_value_scheme`] — the only two entry points that persist a scheme
/// past their one-shot session.
fn close_thunk_scheme(
    inferencer: &mut infer::Inferencer,
    cty: CompTy,
    invariant: &'static str,
) -> Scheme {
    let thunk_ty = Ty::Thunk(Box::new(cty));
    inferencer.ctx.settle_pending_indexes();
    inferencer.ctx.settle_pending_labels(None);
    let scheme = generalize(
        &inferencer.ctx.unifier,
        &TyEnv::new(),
        &FreeVars::new(),
        &thunk_ty,
    );
    let scheme = Arc::unwrap_or_clone(settle_weak(&inferencer.ctx.unifier, Arc::new(scheme)));
    self::generalize::debug_assert_scheme_closed(&inferencer.ctx.unifier, &scheme, invariant);
    scheme
}

/// Type-check `top`, seeding from the live session.
///
/// Infer each phrase in order, extending `TyEnv` at each `Define`, then
/// write back the verdict — each `Define`'s generalised per-name schemes
/// land on its own `Phrase::Define`, not on a shared spine.  Only closed
/// schemes leave — a run's unifier dies with the run and its variable ids
/// restart at zero, so an open scheme from run *N* would alias run
/// *N+1*'s fresh variables.
///
/// A [`ReturnContract`] additionally ascribes the declared table to `top`'s
/// own return value, once inference has finished (`contract::ascribe`).  The
/// *inferred* type is what is checked, so a key misspelled inside a spread is
/// caught with one written out.  A return typed at a variable, and a plugin
/// factory's `return { |opts| … }`, stay on the caller's own runtime door,
/// which dispatches off the same table.
///
/// # Errors
/// Every diagnostic inference collected, whenever that list is non-empty.
/// Inference alone judges; the write-back pass runs only on a program it
/// accepted, and places the coercions that verdict implies.
pub fn typecheck(
    top: &Toplevel,
    schemes: SessionSchemes,
    contract: Option<ReturnContract>,
) -> Result<Toplevel, Vec<TypeError>> {
    let (mut ctx, mut env) = seeded_session(schemes);

    let (mut phrase_schemes, tail) = infer::infer_toplevel(&mut ctx, &mut env, top);
    ctx.settle_pending_indexes();
    ctx.settle_pending_labels(None);
    if ctx.errors.is_empty()
        && let Some(table) = contract
    {
        contract::ascribe(&mut ctx, top.phrases.last(), tail, table);
    }
    if !ctx.errors.is_empty() {
        return Err(ctx.errors);
    }
    for (_, scheme) in phrase_schemes.iter_mut().flatten() {
        *scheme = settle_weak(&ctx.unifier, Arc::clone(scheme));
    }

    ctx.snapshot_sites();
    Ok(annotate::annotate_toplevel(top, &mut ctx, phrase_schemes))
}

/// The (name, scheme) pairs on an *annotated* [`Toplevel`]'s `Phrase::Define`s,
/// in phrase order — `Define.schemes` needs no tree walk, since every phrase
/// already carries its own.  `bake_prelude` is a one-time build-time pass, so
/// unwrapping each `Arc<Scheme>` back to an owned `Scheme` here — for
/// `BakedPrelude::schemes`' postcard blob — costs nothing worth avoiding.
fn harvest_schemes(top: &Toplevel) -> Vec<(String, Scheme)> {
    top.phrases
        .iter()
        .flat_map(|phrase| match &phrase.item {
            Phrase::Define { schemes, .. } => schemes
                .iter()
                .map(|(name, scheme)| (name.clone(), (**scheme).clone()))
                .collect(),
            Phrase::Run(_) => Vec::new(),
        })
        .collect()
}

/// Type-check the prelude IR, returning the annotated [`Toplevel`] and the
/// schemes on its `Phrase::Define`s.
///
/// Callers — `boot::bake_prelude_to_out_dir`, from each host's build script
/// — serialise the *annotated* prelude, so the phrase blob and the scheme
/// blob come out of one checked pass and running the prelude installs each
/// binding's scheme beside its value.  A prelude binding named after a
/// native seeds and shadows like any other.
///
/// # Panics
/// If the prelude fails to type-check, reporting the errors.
pub fn bake_prelude(top: &Toplevel) -> (Toplevel, Vec<(String, Scheme)>) {
    let seed = SessionSchemes::default();
    let annotated = match typecheck(top, seed, None) {
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
/// so a later run's check can be seeded with it.  The arm stands in for the
/// head it names, so its result is that head's (`Inferencer::stands_in`).
///
/// # Errors
/// The arm's result is not what its head returns.
pub(crate) fn alias_arm_scheme(
    head: &str,
    param: &crate::ir::IrPattern,
    body: &Comp,
    schemes: SessionSchemes,
) -> Result<Scheme, Box<TypeError>> {
    one_shot_inference(schemes, |inferencer| {
        let cty = inferencer.infer_alias_arm(Some(param), body);
        inferencer.stands_in(head, &cty)?;
        Ok(close_thunk_scheme(
            inferencer,
            cty,
            "alias-arm scheme must leave no variable free",
        ))
    })
}

/// The computed catch-all's vet — `within [handler: $k]`, where the arm is a
/// runtime value with no literal thunk for the checker to have already held
/// to [`infer::Inferencer::infer_catch_all`]'s shape. Closed against its own
/// unifier, the way [`alias_arm_scheme`] is; no scheme persists, a catch-all
/// frame not outliving its run.
///
/// # Errors
/// The catch-all, which stands in for every command, returns something but `()`.
pub(crate) fn catch_all_stands_in(
    body: &Comp,
    schemes: SessionSchemes,
) -> Result<(), Box<TypeError>> {
    one_shot_inference(schemes, |inferencer| {
        let cty = inferencer.infer_catch_all(body);
        inferencer.catch_all_stands_in(&cty)
    })
}

/// The scheme for a value binding (`Shell::bind_value`, `Shell::register_hook`),
/// inferred under the ordinary value/function-application convention.
///
/// A lambda is a `Fun(param, body)` whose parameter binds an independent
/// value type, not the argv list an alias arm is forced onto.  Closed
/// against its own unifier; with no head to pin to, the scheme comes back
/// directly.
pub(crate) fn binding_value_scheme(
    param: Option<&crate::ir::IrPattern>,
    body: &Comp,
    schemes: SessionSchemes,
) -> Scheme {
    one_shot_inference(schemes, |inferencer| {
        let cty = inferencer.infer_binding_value(param, body);
        close_thunk_scheme(
            inferencer,
            cty,
            "binding-value scheme must leave no variable free",
        )
    })
}
