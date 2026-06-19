//! Hindley-Milner type inference for ral.
//!
//! Types sit at the CBPV IR level — on Val and Comp after elaboration.
//! Value types (A) describe data; computation types (B) describe effectful
//! computations with pipeline modes.  Polymorphism by let-generalisation.
//!
//! The two sorts:
//!
//!   Value ::= Unit | Bool | Int | Float | String | [Value] | [String:Value]
//!          | {l₁:V₁, …, lₙ:Vₙ | row}   -- record (row-polymorphic)
//!          | {Comp} | Handle | TypeVar
//!   Comp ::= F[I,O] Value | Value → Comp | CompVar
//!   I,O ::= ∅ | Bytes | IOVar
//!
//! Generalisation happens at Bind (let) nodes.  Recursive bindings (LetRec,
//! Rec) are given monomorphic types to prevent unsound generalisation.

pub mod builtins;
mod env;
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
pub use self::fmt::{FmtCtx, fmt_mode, fmt_mode_ctx, fmt_scheme, fmt_ty, fmt_ty_ctx};
pub use self::scheme::{CachedFreeVars, CompDiff, Scheme, TypeError, TypeErrorKind};
pub use self::ty::{CompTy, CompTyVar, ModeVar, PipeMode, PipeSpec, Row, RowVar, Ty, TyVar};
pub use self::unify::Unifier;

use self::generalize::generalize;
use crate::ir::{
    Comp, CompKind, Exec, IrPattern, RedirectV, ScopeOp, Val, ValListElem, ValMapEntry,
    ValRedirectTarget,
};
use crate::mode::Wire;
use crate::source::Spanned;
use crate::syntax::ast::MapPatternEntry;
use std::sync::Arc;

/// The live session as the checker sees it: every top-level name with
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
        SessionSchemes {
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
pub fn typecheck(comp: &Comp, schemes: SessionSchemes) -> Result<Comp, Vec<TypeError>> {
    let mut ctx = InferCtx::new();
    let mut env = TyEnv::new();
    seed_env(&mut env, schemes);

    infer::infer_comp(&mut ctx, &mut env, comp);
    if ctx.errors.is_empty() {
        Ok(annotate(comp, &mut ctx, true))
    } else {
        Err(ctx.errors)
    }
}

/// Rebuild `comp` with the checker's mode verdicts written into the IR:
/// each `Bind`'s generalised scheme and ground RHS output mode, and each
/// pipeline stage's ground [`Wire`].
///
/// Schemes are written on the top-level spine only — the statement
/// positions whose `Bind`s install into the persistent scope at
/// evaluation: a `Seq`'s parts and a `Bind`'s `rest`.  A `Bind` under a
/// thunk or an `if` branch evaluates in a block scope and never installs
/// into the session, so it carries no scheme.  The `spine` flag tracks
/// that position: only those two descents keep it set.
///
/// The recorded pre-generalisation type is closed by generalising against
/// the empty environment: that resolves it against the final unifier and
/// quantifies every residual variable, which is exactly the closure
/// condition a scheme must satisfy to survive into the next turn's check.
///
/// Wires and RHS output modes are written everywhere they were recorded,
/// at any depth — inside thunk bodies, branches, pipeline stages.  A node
/// inference never visited keeps the elaborator's `Empty` placeholder.
/// Every other field, and every span, is rebuilt bit-identically.
fn annotate(comp: &Comp, ctx: &mut InferCtx, spine: bool) -> Comp {
    let item = match &comp.item {
        CompKind::Seq(parts) => CompKind::Seq(
            parts
                .iter()
                .map(|part| Arc::new(annotate(part, ctx, spine)))
                .collect(),
        ),
        CompKind::Bind {
            comp: rhs,
            pattern,
            rest,
            rhs_output,
            ..
        } => {
            let scheme = spine
                .then(|| ctx.bind_tys.get(&(comp as *const Comp as usize)).cloned())
                .flatten()
                .map(|ty| {
                    let scheme = generalize(&mut ctx.unifier, &TyEnv::new(), &ty);
                    debug_assert!(
                        self::generalize::scheme_is_closed(&mut ctx.unifier, &scheme),
                        "top-level Bind scheme must leave no variable free"
                    );
                    Box::new(scheme)
                });
            // A node inference never visited keeps the elaborator's
            // placeholder; every other node takes the checker's verdict.
            let rhs_output = ctx
                .bind_outputs
                .get(&(comp as *const Comp as usize))
                .copied()
                .map_or(*rhs_output, |m| ctx.ground(m));
            CompKind::Bind {
                comp: Arc::new(annotate(rhs, ctx, false)),
                pattern: annotate_pattern(pattern, ctx),
                rest: Arc::new(annotate(rest, ctx, spine)),
                scheme,
                rhs_output,
            }
        }
        CompKind::Pipeline { stages, wires } => {
            let wires = stages
                .iter()
                .zip(wires)
                .map(|(stage, placeholder)| {
                    ctx.stage_specs
                        .get(&(stage.as_ref() as *const Comp as usize))
                        .copied()
                        .map_or(*placeholder, |spec| Wire {
                            input: ctx.ground(spec.input),
                            output: ctx.ground(spec.output),
                        })
                })
                .collect();
            CompKind::Pipeline {
                stages: stages
                    .iter()
                    .map(|stage| Arc::new(annotate(stage, ctx, false)))
                    .collect(),
                wires,
            }
        }
        CompKind::Lam { param, body } => CompKind::Lam {
            param: annotate_pattern(param, ctx),
            body: Arc::new(annotate(body, ctx, false)),
        },
        CompKind::App { head, args } => CompKind::App {
            head: Arc::new(annotate(head, ctx, false)),
            args: annotate_args(args, ctx),
        },
        CompKind::Chain(parts) => CompKind::Chain(
            parts
                .iter()
                .map(|part| Arc::new(annotate(part, ctx, false)))
                .collect(),
        ),
        CompKind::If { cond, then, else_ } => CompKind::If {
            cond: annotate_spanned_val(cond, ctx),
            then: Arc::new(annotate(then, ctx, false)),
            else_: Arc::new(annotate(else_, ctx, false)),
        },
        CompKind::Force(value) => CompKind::Force(annotate_val(value, ctx)),
        CompKind::Return(value) => CompKind::Return(annotate_val(value, ctx)),
        CompKind::Exec(e) => CompKind::Exec(Exec {
            head: e.head.clone(),
            args: annotate_args(&e.args, ctx),
            redirects: e
                .redirects
                .iter()
                .map(|r| annotate_redirect(r, ctx))
                .collect(),
        }),
        CompKind::Binary(op, lhs, rhs) => {
            CompKind::Binary(*op, annotate_val(lhs, ctx), annotate_val(rhs, ctx))
        }
        CompKind::Not(value) => CompKind::Not(annotate_val(value, ctx)),
        CompKind::Index { target, keys } => CompKind::Index {
            target: annotate_val(target, ctx),
            keys: keys.iter().map(|k| annotate_spanned_val(k, ctx)).collect(),
        },
        CompKind::Interpolation(parts) => {
            CompKind::Interpolation(parts.iter().map(|v| annotate_val(v, ctx)).collect())
        }
        CompKind::LetRec { slot, bindings } => CompKind::LetRec {
            slot: *slot,
            bindings: Arc::new(
                bindings
                    .iter()
                    .map(|(name, value)| (name.clone(), annotate_val(value, ctx)))
                    .collect(),
            ),
        },
        CompKind::Case { scrutinee, table } => CompKind::Case {
            scrutinee: annotate_spanned_val(scrutinee, ctx),
            table: annotate_spanned_val(table, ctx),
        },
        CompKind::Scope(op) => CompKind::Scope(annotate_scope(op, ctx)),
    };
    Spanned::with_span(comp.span, item)
}

/// Annotate the [`Val`] category — descending into thunk bodies (always
/// non-spine) and the values nested in lists, maps, and variants.
fn annotate_val(val: &Val, ctx: &mut InferCtx) -> Val {
    match val {
        Val::Thunk(comp) => Val::Thunk(Arc::new(annotate(comp, ctx, false))),
        Val::List(elems) => Val::List(elems.iter().map(|e| annotate_list_elem(e, ctx)).collect()),
        Val::Map(entries) => Val::Map(
            entries
                .iter()
                .map(|e| match e {
                    ValMapEntry::Entry(k, v) => {
                        ValMapEntry::Entry(annotate_val(k, ctx), annotate_val(v, ctx))
                    }
                    ValMapEntry::Spread(v) => ValMapEntry::Spread(annotate_val(v, ctx)),
                })
                .collect(),
        ),
        Val::Variant { label, payload } => Val::Variant {
            label: label.clone(),
            payload: payload.as_ref().map(|p| Box::new(annotate_val(p, ctx))),
        },
        Val::Unit
        | Val::String(_)
        | Val::Int(_)
        | Val::Float(_)
        | Val::Bool(_)
        | Val::Variable(_)
        | Val::TildePath(_) => val.clone(),
    }
}

/// Rebuild a [`Spanned<Val>`] sub-position, keeping its span.
fn annotate_spanned_val(value: &Spanned<Val>, ctx: &mut InferCtx) -> Spanned<Val> {
    Spanned::with_span(value.span, annotate_val(&value.item, ctx))
}

/// Annotate one list/argument element, descending into its value.
fn annotate_list_elem(elem: &ValListElem, ctx: &mut InferCtx) -> ValListElem {
    match elem {
        ValListElem::Single(v) => ValListElem::Single(annotate_val(v, ctx)),
        ValListElem::Spread(v) => ValListElem::Spread(annotate_val(v, ctx)),
    }
}

fn annotate_args(args: &crate::ir::Args, ctx: &mut InferCtx) -> crate::ir::Args {
    args.iter()
        .map(|e| Spanned::with_span(e.span, annotate_list_elem(&e.item, ctx)))
        .collect()
}

fn annotate_redirect(redirect: &RedirectV, ctx: &mut InferCtx) -> RedirectV {
    RedirectV {
        fd: redirect.fd,
        mode: redirect.mode,
        target: match &redirect.target {
            ValRedirectTarget::File(v) => ValRedirectTarget::File(annotate_val(v, ctx)),
            ValRedirectTarget::Fd(n) => ValRedirectTarget::Fd(*n),
        },
    }
}

fn annotate_scope(op: &ScopeOp, ctx: &mut InferCtx) -> ScopeOp {
    match op {
        ScopeOp::Try { body, handler } => ScopeOp::Try {
            body: annotate_val(body, ctx),
            handler: annotate_val(handler, ctx),
        },
        ScopeOp::Guard { body, cleanup } => ScopeOp::Guard {
            body: annotate_val(body, ctx),
            cleanup: annotate_val(cleanup, ctx),
        },
        ScopeOp::Within { opts, body } => ScopeOp::Within {
            opts: annotate_val(opts, ctx),
            body: annotate_val(body, ctx),
        },
        ScopeOp::Grant { caps, body } => ScopeOp::Grant {
            caps: annotate_val(caps, ctx),
            body: annotate_val(body, ctx),
        },
        ScopeOp::Audit { body } => ScopeOp::Audit {
            body: annotate_val(body, ctx),
        },
        ScopeOp::Redirect { body, redirects } => ScopeOp::Redirect {
            body: Arc::new(annotate(body, ctx, false)),
            redirects: redirects
                .iter()
                .map(|r| annotate_redirect(r, ctx))
                .collect(),
        },
    }
}

/// Annotate a pattern's elaborated map-default comps — the only `Comp`
/// positions a pattern carries.  Every default body is non-spine.
fn annotate_pattern(pattern: &IrPattern, ctx: &mut InferCtx) -> IrPattern {
    match pattern {
        IrPattern::Wildcard | IrPattern::Name(_) => pattern.clone(),
        IrPattern::List { elems, rest } => IrPattern::List {
            elems: elems.iter().map(|p| annotate_pattern(p, ctx)).collect(),
            rest: rest.clone(),
        },
        IrPattern::Map(entries) => IrPattern::Map(
            entries
                .iter()
                .map(|entry| MapPatternEntry {
                    key: entry.key.clone(),
                    pattern: annotate_pattern(&entry.pattern, ctx),
                    default: entry
                        .default
                        .as_ref()
                        .map(|d| Arc::new(annotate(d, ctx, false))),
                })
                .collect(),
        ),
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
/// The prelude elaborates to a top-level `Seq([Bind, Bind, …])`.  Normal
/// inference of a `Seq` runs inside a fresh `TyEnv` frame
/// (`Inferencer::infer_comp`'s `Seq` arm) so that aliases and lets
/// introduced inside a `{…}` block don't leak past the block's lexical
/// extent.  That framing is wrong here: we *want* the prelude's top-level
/// bindings to survive into the harvested scheme list.  So when `comp` is
/// a `Seq`, we hand its parts straight to
/// `infer_seq_with_alias_bindings` without the surrounding `with_scope`,
/// so each `Bind` accumulates into the root scope of `env`.  A non-`Seq`
/// `comp` (a single-statement prelude) goes through normal inference —
/// `Bind` itself does no push/pop, so the binding lands in the root
/// scope directly.
pub fn bake_prelude(comp: &Comp) -> (Comp, Vec<(String, Scheme)>) {
    let mut ctx = InferCtx::new();
    let mut env = TyEnv::new();
    seed_builtins(&mut ctx.unifier, &mut env);
    {
        let mut inferencer = infer::Inferencer {
            ctx: &mut ctx,
            env: &mut env,
        };
        match &comp.item {
            CompKind::Seq(parts) => {
                inferencer.infer_seq_with_alias_bindings(parts, Ty::Unit);
            }
            _ => {
                inferencer.infer_comp(comp);
            }
        }
    }
    let annotated = annotate(comp, &mut ctx, true);
    let schemes = harvest_schemes(&annotated)
        .into_iter()
        .filter(|(name, _)| !crate::builtins::is_builtin(name))
        .collect();
    (annotated, schemes)
}

/// The scheme stored on a persistent alias frame at install
/// (`Shell::install_alias`): the arm body for head `head` inferred under
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
    debug_assert!(
        self::generalize::scheme_is_closed(&mut ctx.unifier, &scheme),
        "alias-arm scheme must leave no variable free"
    );
    Ok(scheme)
}

/// The scheme stored for a callable installed as a lexical scope binding
/// (`Shell::bind_value`, the rc `bindings:` path): the value's type
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
    debug_assert!(
        self::generalize::scheme_is_closed(&mut ctx.unifier, &scheme),
        "binding-value scheme must leave no variable free"
    );
    scheme
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
