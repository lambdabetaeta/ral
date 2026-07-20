//! The write-back pass: rebuild a checked comp with the inferencer's mode
//! verdicts, generalised schemes, and ground pipeline wires written into the
//! IR.  Runs after inference succeeds, driven from [`super::typecheck`].

use super::env::{InferCtx, TyEnv};
use super::generalize::generalize;
use crate::ir::{
    Comp, CompKind, Exec, IrPattern, RedirectV, ScopeOp, Val, ValListElem, ValMapEntry,
    ValRedirectTarget,
};
use crate::mode::Wire;
use crate::source::Spanned;
use crate::syntax::ast::MapPatternEntry;
use std::sync::Arc;

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
pub(super) fn annotate(comp: &Comp, ctx: &mut InferCtx, spine: bool) -> Comp {
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
                .then(|| {
                    ctx.bind_tys
                        .get(&(std::ptr::from_ref::<Comp>(comp) as usize))
                        .cloned()
                })
                .flatten()
                .map(|ty| {
                    let scheme = generalize(&mut ctx.unifier, &TyEnv::new(), &ty);
                    super::generalize::debug_assert_scheme_closed(
                        &mut ctx.unifier,
                        &scheme,
                        "top-level Bind scheme must leave no variable free",
                    );
                    Box::new(scheme)
                });
            // A node inference never visited keeps the elaborator's
            // placeholder; every other node takes the checker's verdict.
            let rhs_output = ctx
                .bind_outputs
                .get(&(std::ptr::from_ref::<Comp>(comp) as usize))
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
        CompKind::Pipeline {
            stages,
            wires,
            stage_types,
        } => {
            let wires = stages
                .iter()
                .zip(wires)
                .map(|(stage, placeholder)| {
                    ctx.stage_specs
                        .get(&(std::ptr::from_ref::<Comp>(stage.as_ref()) as usize))
                        .copied()
                        .map_or(*placeholder, |spec| Wire {
                            input: ctx.ground(spec.input),
                            output: ctx.ground(spec.output),
                        })
                })
                .collect();
            // Resolve each stage's recorded value type against the final
            // unifier; a stage inference never visited keeps the elaborator's
            // `Unit` placeholder.
            let stage_types = stages
                .iter()
                .zip(stage_types)
                .map(|(stage, placeholder)| {
                    ctx.stage_types
                        .get(&(std::ptr::from_ref::<Comp>(stage.as_ref()) as usize))
                        .cloned()
                        .map_or_else(|| placeholder.clone(), |ty| ctx.unifier.resolve_ty(&ty))
                })
                .collect();
            CompKind::Pipeline {
                stages: stages
                    .iter()
                    .map(|stage| Arc::new(annotate(stage, ctx, false)))
                    .collect(),
                wires,
                stage_types,
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
