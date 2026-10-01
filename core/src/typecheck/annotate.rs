//! Write-back pass: rebuild a checked comp with the inferencer's verdicts —
//! generalised schemes, boundary sites, and `capture` coercions — over the
//! tree that was inferred, using [`InferCtx`]'s node-address-keyed side maps.
//!
//! The walk is a plain structural rebuild: the commands inference recorded
//! as captured are wrapped in [`captured_string`] where they stand, and a
//! captured value in hand is η-wrapped into a block that captures its call.

use super::env::{InferCtx, comp_key, val_key};
use crate::ir::{
    Assembly, CaseArm, CommandName, CommandWord, Comp, CompKind, DefineSchemes, Exec, GroupNode,
    HandlerArmV, IrPattern, Name, OptionsV, Phrase, Toplevel, Val, ValListElem, ValMapEntry,
    ValRecordEntry,
};
use crate::source::{Span, Spanned};
use crate::syntax::ast::Redirects;
use crate::types::Site;
use std::sync::Arc;

/// Rebuild `group`, memoized by its source `Arc`'s identity: every `Rec`
/// projection of one group is walked here once, so all of them keep sharing
/// one rebuilt node, as the elaborator built them.
fn annotate_rec_group(group: &Arc<GroupNode>, ctx: &mut InferCtx, eta: bool) -> Arc<GroupNode> {
    let key = Arc::as_ptr(group).cast::<()>();
    if let Some(rebuilt) = ctx.rec_group_rebuilds.get(&key) {
        return Arc::clone(rebuilt);
    }
    let rebuilt = GroupNode::new(
        group
            .shape()
            .iter()
            .map(|(name, m)| (name.clone(), Arc::new(annotate_comp(m, ctx, eta))))
            .collect::<Vec<_>>()
            .into(),
    );
    ctx.rec_group_rebuilds.insert(key, Arc::clone(&rebuilt));
    rebuilt
}

/// The whole of the capture coercion: `cap M to d. decode d`.
/// The kernel's `decode` takes a value, so the lossy, partial step that reads
/// the capture's bytes as text needs a bind to reach it. Both nodes take
/// `body`'s span, so a decode failure still names the expression the user
/// wrote.
///
/// No command: what the checker composes here means the same thing in every
/// environment, because the bind's name is synthetic — nothing user code can
/// rebind or observe.
fn captured_string(body: Comp, ctx: &mut InferCtx) -> CompKind {
    let span = body.span;
    let name: Name = ctx.fresh_name("decode").into();
    CompKind::Bind {
        comp: Arc::new(Spanned::with_span(span, CompKind::Capture(Arc::new(body)))),
        pattern: Arc::new(IrPattern::Name(name.clone())),
        rest: Arc::new(Spanned::with_span(
            span,
            CompKind::Decode(Val::Variable(name)),
        )),
    }
}

/// Rebuild `comp`: the general recursive walk a thunked value gets.
pub(super) fn annotate(comp: &Comp, ctx: &mut InferCtx) -> Comp {
    annotate_comp(comp, ctx, false)
}

/// Rebuild the right-hand side of a `Bind` or `Define`, then η-expand it by
/// the arity recorded for the *original* `rhs`'s address in
/// `ctx.rhs_arrow_arity`.  `Bind` never generalises a scheme — that lives on
/// `Phrase::Define` alone, one per bound name.
fn annotate_rhs(rhs: &Arc<Comp>, ctx: &mut InferCtx, eta: bool) -> Arc<Comp> {
    let annotated = annotate_comp(rhs, ctx, eta);
    let arity = if eta {
        ctx.rhs_arrow_arity.get(&comp_key(rhs)).copied()
    } else {
        None
    };
    debug_assert!(
        arity.is_none() || !ctx.captured.contains(&comp_key(rhs)),
        "a function-typed RHS is never captured"
    );
    match arity {
        Some(arity) => Arc::new(eta_expand_arrow(annotated, ctx, arity)),
        None => Arc::new(annotated),
    }
}

/// `{ |x₁…xₙ| cap (!v x₁ … xₙ) to d. decode d }`: a captured value in hand,
/// its call wrapped where a literal block's body would be.
fn captured_block(value: Val, arity: usize, span: Option<Span>, ctx: &mut InferCtx) -> Val {
    let params: Vec<Name> = (0..arity).map(|_| ctx.fresh_name("eta").into()).collect();
    let forced = Spanned::with_span(span, CompKind::Force(value));
    let call = if params.is_empty() {
        forced.item
    } else {
        CompKind::App {
            head: Arc::new(forced),
            args: params
                .iter()
                .map(|param| ValListElem::Single(Spanned::synthetic(Val::Variable(param.clone()))))
                .collect(),
        }
    };
    let body = captured_string(Spanned::with_span(span, call), ctx);
    lambda(span, params, body)
}

/// Rebuild an arrow-typed RHS of curried arity `arity` as
/// `Return(Thunk(λx₁. … λxₙ. App { head, args }))`, flattening `rhs` into
/// the `App`'s own head when it is itself an `App` — so every
/// function-typed thunk's body is a syntactic `Lam` ([`Comp::arrow`]).
fn eta_expand_arrow(rhs: Comp, ctx: &mut InferCtx, arity: usize) -> Comp {
    let span = rhs.span;
    let params: Vec<Name> = (0..arity).map(|_| ctx.fresh_name("eta").into()).collect();
    let applied = params
        .iter()
        .map(|param| ValListElem::Single(Spanned::synthetic(Val::Variable(param.clone()))));
    let call = match rhs.item {
        CompKind::App { head, mut args } => {
            args.extend(applied);
            CompKind::App { head, args }
        }
        other => CompKind::App {
            head: Arc::new(Spanned::with_span(span, other)),
            args: applied.collect(),
        },
    };
    Spanned::with_span(span, CompKind::Return(lambda(span, params, call)))
}

/// `{ |x₁| … |xₙ| body }`.
fn lambda(span: Option<Span>, params: Vec<Name>, body: CompKind) -> Val {
    Val::thunk(Arc::new(lambda_comp(span, params, body)))
}

/// `λx₁. … λxₙ. body`, as a computation.
fn lambda_comp(span: Option<Span>, params: Vec<Name>, body: CompKind) -> Comp {
    params
        .into_iter()
        .rev()
        .fold(Spanned::with_span(span, body), |body, param| {
            Spanned::with_span(
                span,
                CompKind::Lam {
                    param: IrPattern::Name(param),
                    body: Arc::new(body),
                },
            )
        })
}

/// A boundary builtin held as a value, as `{ |x…| name x… }`: the block's call
/// is an ordinary saturated one, so it carries the site.
fn boundary_block(
    name: &str,
    arity: usize,
    site: Arc<Site>,
    span: Option<Span>,
    ctx: &mut InferCtx,
) -> Val {
    let params: Vec<Name> = (0..arity).map(|_| ctx.fresh_name("eta").into()).collect();
    let args = params
        .iter()
        .map(|param| ValListElem::Single(Spanned::synthetic(Val::Variable(param.clone()))))
        .collect();
    let call = CompKind::Exec(Exec {
        head: CommandWord::Name(CommandName::Bare(name.into())),
        args,
        redirects: Redirects::default(),
        site: Some(site),
    });
    lambda(span, params, call)
}

fn annotate_comp(comp: &Comp, ctx: &mut InferCtx, eta: bool) -> Comp {
    let item = match &comp.item {
        CompKind::Bind {
            comp: rhs,
            pattern,
            rest,
        } => CompKind::Bind {
            comp: annotate_rhs(rhs, ctx, eta),
            pattern: Arc::clone(pattern),
            rest: Arc::new(annotate_comp(rest, ctx, eta)),
        },
        CompKind::Pipeline {
            stages,
            stage_types,
        } => {
            let stage_types = stages
                .iter()
                .zip(stage_types)
                .map(|(stage, placeholder)| {
                    ctx.stage_types
                        .get(&comp_key(stage))
                        .cloned()
                        .map_or_else(|| placeholder.clone(), |ty| ctx.unifier.resolve_ty(&ty))
                })
                .collect();
            CompKind::Pipeline {
                stages: stages
                    .iter()
                    .map(|stage| Arc::new(annotate_comp(stage, ctx, eta)))
                    .collect(),
                stage_types,
            }
        }
        CompKind::Lam { param, body } => CompKind::Lam {
            param: param.clone(),
            body: Arc::new(annotate_comp(body, ctx, eta)),
        },
        CompKind::App { head, args } => CompKind::App {
            head: Arc::new(annotate_comp(head, ctx, eta)),
            args: annotate_args(args, ctx),
        },
        CompKind::Force(value) => CompKind::Force(annotate_val(value, ctx)),
        CompKind::Return(value) => CompKind::Return(annotate_val(value, ctx)),
        CompKind::Assemble(assembly) => CompKind::Assemble(annotate_assembly(assembly, ctx)),
        CompKind::Exec(e) => annotate_exec(comp, e, ctx),
        CompKind::Binary(op, lhs, rhs) => {
            CompKind::Binary(*op, annotate_val(lhs, ctx), annotate_val(rhs, ctx))
        }
        CompKind::Negate(value) => CompKind::Negate(annotate_val(value, ctx)),
        CompKind::Not(value) => CompKind::Not(annotate_val(value, ctx)),
        CompKind::Index { target, keys } => CompKind::Index {
            target: annotate_val(target, ctx),
            keys: keys.iter().map(|k| annotate_spanned_val(k, ctx)).collect(),
        },
        CompKind::Interpolation(parts) => {
            CompKind::Interpolation(parts.iter().map(|v| annotate_val(v, ctx)).collect())
        }
        CompKind::Rec { group, index } => CompKind::Rec {
            group: annotate_rec_group(group, ctx, eta),
            index: *index,
        },
        CompKind::Observe(reg) => CompKind::Observe(reg.clone()),
        CompKind::If { cond, then, else_ } => CompKind::If {
            cond: annotate_spanned_val(cond, ctx),
            then: annotate_spanned_val(then, ctx),
            else_: annotate_spanned_val(else_, ctx),
        },
        CompKind::Case { scrutinee, arms } => CompKind::Case {
            scrutinee: annotate_spanned_val(scrutinee, ctx),
            arms: arms
                .iter()
                .map(|arm| CaseArm {
                    tag: arm.tag.clone(),
                    body: annotate_spanned_val(&arm.body, ctx),
                })
                .collect(),
        },
        CompKind::Try { body, handler } => CompKind::Try {
            body: annotate_val(body, ctx),
            handler: annotate_val(handler, ctx),
        },
        CompKind::Guard { body, cleanup } => CompKind::Guard {
            body: annotate_val(body, ctx),
            cleanup: annotate_val(cleanup, ctx),
        },
        CompKind::Within {
            opts,
            handlers,
            body,
        } => CompKind::Within {
            opts: annotate_options(opts, ctx),
            handlers: handlers.as_ref().map(|arms| {
                arms.iter()
                    .map(|arm| HandlerArmV {
                        name: arm.name.clone(),
                        value: annotate_spanned_val(&arm.value, ctx),
                    })
                    .collect()
            }),
            body: annotate_val(body, ctx),
        },
        CompKind::Grant { caps, body } => CompKind::Grant {
            caps: annotate_options(caps, ctx),
            body: annotate_val(body, ctx),
        },
        CompKind::Audit { body } => CompKind::Audit {
            body: annotate_val(body, ctx),
        },
        CompKind::Redirect { body, redirects } => CompKind::Redirect {
            body: Arc::new(annotate_comp(body, ctx, eta)),
            redirects: redirects.map(|v| annotate_val(v, ctx)),
        },
        CompKind::Capture(body) => CompKind::Capture(Arc::new(annotate_comp(body, ctx, eta))),
        CompKind::Decode(value) => CompKind::Decode(annotate_val(value, ctx)),
    };
    let item = if ctx.captured.contains(&comp_key(comp)) {
        captured_string(Spanned::with_span(comp.span, item), ctx)
    } else {
        item
    };
    Spanned::with_span(comp.span, item)
}

/// An `Exec`: its site from the checker, η-expanded when it is an
/// under-applied boundary.
fn annotate_exec(comp: &Comp, e: &Exec, ctx: &mut InferCtx) -> CompKind {
    let mut exec = Exec {
        head: e.head.clone(),
        args: annotate_args(&e.args, ctx),
        redirects: e.redirects.map(|v| annotate_val(v, ctx)),
        site: ctx.sites.get(&comp_key(comp)).cloned(),
    };
    // An under-applied boundary is a boundary held as a value: a block of the
    // saturated call, wherever it stands.
    match ctx.boundary_missing.get(&comp_key(comp)).copied() {
        Some(missing) => {
            let params: Vec<Name> = (0..missing).map(|_| ctx.fresh_name("eta").into()).collect();
            exec.args.extend(params.iter().map(|param| {
                ValListElem::Single(Spanned::synthetic(Val::Variable(param.clone())))
            }));
            lambda_comp(comp.span, params, CompKind::Exec(exec)).item
        }
        None => CompKind::Exec(exec),
    }
}

fn annotate_val(val: &Val, ctx: &mut InferCtx) -> Val {
    annotate_val_at(val, None, ctx)
}

/// [`annotate_val`] for a value written at `span`, which the block a boundary
/// reference or a captured value becomes is given, so its errors point where
/// the value is.
fn annotate_val_at(val: &Val, span: Option<Span>, ctx: &mut InferCtx) -> Val {
    if let Some(value) = ctx.boundary_values.get(&val_key(val))
        && let Some(site) = ctx.sites.get(&val_key(val)).cloned()
    {
        let (name, arity) = (value.name.clone(), value.arity);
        return boundary_block(&name, arity, site, span, ctx);
    }
    let rebuilt = rebuild_val(val, ctx);
    match ctx.captured_vals.get(&val_key(val)).copied() {
        Some(arity) => captured_block(rebuilt, arity, span, ctx),
        None => rebuilt,
    }
}

/// The structural rebuild of a value.
fn rebuild_val(val: &Val, ctx: &mut InferCtx) -> Val {
    match val {
        Val::Thunk(comp) => Val::thunk(Arc::new(annotate(comp.shape(), ctx))),
        Val::List(elems) => Val::list(
            elems
                .shape()
                .iter()
                .map(|v| annotate_spanned_val(v, ctx))
                .collect::<Vec<_>>(),
        ),
        Val::Record(entries) => Val::record(
            entries
                .shape()
                .iter()
                .map(|(k, v)| (k.clone(), annotate_spanned_val(v, ctx)))
                .collect::<Vec<_>>(),
        ),
        Val::Map(entries) => Val::map(
            entries
                .shape()
                .iter()
                .map(|(k, v)| (k.clone(), annotate_spanned_val(v, ctx)))
                .collect::<Vec<_>>(),
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
        | Val::Variable(_) => val.clone(),
    }
}

fn annotate_spanned_val(value: &Spanned<Val>, ctx: &mut InferCtx) -> Spanned<Val> {
    Spanned::with_span(value.span, annotate_val_at(&value.item, value.span, ctx))
}

fn annotate_list_elem(elem: &ValListElem, ctx: &mut InferCtx) -> ValListElem {
    match elem {
        ValListElem::Single(v) => ValListElem::Single(annotate_spanned_val(v, ctx)),
        ValListElem::Spread(v) => ValListElem::Spread(annotate_spanned_val(v, ctx)),
    }
}

fn annotate_args(args: &crate::ir::Args, ctx: &mut InferCtx) -> crate::ir::Args {
    args.iter().map(|e| annotate_list_elem(e, ctx)).collect()
}

fn annotate_assembly(assembly: &Assembly, ctx: &mut InferCtx) -> Assembly {
    match assembly {
        Assembly::List(elems) => Assembly::List(annotate_args(elems, ctx)),
        Assembly::Record(entries) => Assembly::Record(
            entries
                .iter()
                .map(|e| match e {
                    ValRecordEntry::Field(label, v) => {
                        ValRecordEntry::Field(label.clone(), annotate_spanned_val(v, ctx))
                    }
                    ValRecordEntry::Spread(v) => {
                        ValRecordEntry::Spread(annotate_spanned_val(v, ctx))
                    }
                })
                .collect(),
        ),
        Assembly::Map(entries) => Assembly::Map(
            entries
                .iter()
                .map(|e| match e {
                    ValMapEntry::Entry(k, v) => {
                        ValMapEntry::Entry(annotate_val(k, ctx), annotate_spanned_val(v, ctx))
                    }
                    ValMapEntry::Spread(v) => ValMapEntry::Spread(annotate_spanned_val(v, ctx)),
                })
                .collect(),
        ),
    }
}

/// A form's written options, each value annotated where it stands.
fn annotate_options(opts: &OptionsV, ctx: &mut InferCtx) -> OptionsV {
    opts.iter()
        .map(|(name, value)| {
            let value = Spanned::with_span(value.span, annotate_val(&value.item, ctx));
            (name.clone(), value)
        })
        .collect()
}

/// Rebuild a checked [`Toplevel`]: every phrase is walked at `eta = true`,
/// so η-expansion applies throughout, and a `Define`'s RHS is read for its
/// value.  A `Run` is a statement, held ready to run, so nothing η-expands
/// it and nothing captures it.  `schemes`, parallel to `top.phrases`, is
/// [`infer::infer_toplevel`](super::infer::infer_toplevel)'s per-`Define`
/// harvest, written straight onto the rebuilt `Phrase::Define` — `Bind`
/// never carries a scheme, on any path.
pub(super) fn annotate_toplevel(
    top: &Toplevel,
    ctx: &mut InferCtx,
    schemes: Vec<DefineSchemes>,
) -> Toplevel {
    let phrases = top
        .phrases
        .iter()
        .zip(schemes)
        .map(|(phrase, names)| {
            let item = match &phrase.item {
                Phrase::Define { pattern, comp, .. } => Phrase::Define {
                    pattern: Arc::clone(pattern),
                    comp: annotate_rhs(comp, ctx, true),
                    schemes: names,
                },
                Phrase::Run(comp) => Phrase::Run(Arc::new(annotate_comp(comp, ctx, true))),
            };
            Spanned::with_span(phrase.span, item)
        })
        .collect();
    Toplevel {
        phrases,
        admits: ctx.readmits.clone(),
    }
}
