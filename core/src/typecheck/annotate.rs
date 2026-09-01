//! Write-back pass: rebuild a checked comp with the inferencer's verdicts —
//! generalised schemes, a pipeline's yield marker, `capture` coercions — over
//! the tree that was inferred, using [`InferCtx`]'s node-address-keyed side
//! maps.
//!
//! Coercion insertion is one recursive walk carrying a [`Demand`]: `Value`
//! where a boundary reads a payload, `Discard` where one is dropped. `Value`
//! reaching a node whose recorded `result` grounds `Bytes` wraps it with
//! [`captured_string`]; the demand follows the same path the payload rides at
//! run time, so the wrap lands at the leaf that actually owns the bytes.

use super::env::InferCtx;
use super::scheme::Scheme;
use super::ty::GroundRoute;
use crate::ir::{
    Args, CaseArm, Comp, CompKind, Exec, IrPattern, Phrase, PipeYield, RedirectV, Toplevel, Val,
    ValListElem, ValMapEntry, ValRedirectTarget,
};
use crate::source::Spanned;
use crate::syntax::ast::MapPatternEntry;
use std::sync::Arc;

/// What a demand-carrying position wants from the node it reaches.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Demand {
    /// A value is dropped here (statement position, an operand's interior):
    /// never wraps.
    Discard,
    /// A value is read here: a node whose own recorded result grounds
    /// `Bytes` wraps in `Capture`.
    Value,
}

/// Whether to grow the arm/body found (syntactic thunk: recurse into its
/// body; opaque: η-expand) into its byte-payload or subsumed shape.
#[derive(Clone, Copy)]
enum ArmWalk {
    /// `Discard`, or a `Value` demand the node's own result doesn't ground
    /// `Bytes` for: rebuild plain, no `Capture` anywhere.
    Plain,
    /// A byte-payload arm/body: push `Value` demand into it, so the wrap
    /// lands at its own tail leaf.
    Descend,
    /// A subsumed (`∅`-at-`Unit`) arm in a byte-side join: wrap the whole
    /// arm — its own payload is empty, so its `Capture` contributes `""`
    /// and its non-final bytes flush through as effect.
    Wrap,
}

fn comp_key(comp: &Comp) -> usize {
    std::ptr::from_ref::<Comp>(comp) as usize
}

fn val_key(val: &Val) -> usize {
    std::ptr::from_ref::<Val>(val) as usize
}

/// Does `key`'s recorded result ground `Bytes`? Absent or still-unresolved
/// both read `Value`, via [`InferCtx::ground`].
fn bytes_result(ctx: &mut InferCtx, key: usize) -> bool {
    matches!(
        ctx.results
            .get(&key)
            .copied()
            .map(|route| ctx.ground(route)),
        Some(GroundRoute::Bytes)
    )
}

/// The `Val`-keyed analogue of [`bytes_result`], for scope arms.
fn bytes_val_result(ctx: &mut InferCtx, key: usize) -> bool {
    matches!(
        ctx.val_results
            .get(&key)
            .copied()
            .map(|route| ctx.ground(route)),
        Some(GroundRoute::Bytes)
    )
}

/// Is `join`'s payload captured from stdout *and* read as a value here?  This
/// is the one condition that puts a `Capture` inside an arm, so it is also the
/// condition under which every arm must be visible.
fn byte_side_join(join: &Comp, ctx: &mut InferCtx, demand: Demand) -> bool {
    demand == Demand::Value && bytes_result(ctx, comp_key(join))
}

/// The whole of the byte-to-value coercion: `Capture(body) to x. Decode(x)`.
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
    let name = ctx.fresh_name("decode");
    CompKind::Bind {
        comp: Arc::new(Spanned::with_span(span, CompKind::Capture(Arc::new(body)))),
        pattern: Arc::new(IrPattern::Name(name.clone())),
        rest: Arc::new(Spanned::with_span(span, CompKind::Decode(Val::Variable(name)))),
    }
}

/// Rebuild `comp` under `Demand::Discard` — the general recursive walk a
/// thunked value or a pattern default gets, never a `Bind`/`Define` RHS.
pub(super) fn annotate(comp: &Comp, ctx: &mut InferCtx) -> Comp {
    annotate_demand(comp, ctx, false, Demand::Discard)
}

/// `annotate_demand`, but for a position that reads its child's *value* —
/// a `Bind`/`Define`/`Source` RHS, or the toplevel's own tail `Run` — under
/// `demand`, so that S3's η-expansion (`eta_expand_arrow`) can apply after
/// the ordinary rebuild, keyed by the *original* `rhs`'s address in
/// `ctx.rhs_arrow_arity`.  `Bind` never generalises a scheme — that lives on
/// `Phrase::Define` alone, one per bound name.
fn annotate_rhs(rhs: &Arc<Comp>, ctx: &mut InferCtx, eta: bool, demand: Demand) -> Arc<Comp> {
    let annotated = annotate_demand(rhs, ctx, eta, demand);
    let arity = if eta {
        ctx.rhs_arrow_arity.get(&comp_key(rhs)).copied()
    } else {
        None
    };
    match arity {
        Some(arity) => Arc::new(eta_expand_arrow(annotated, ctx, arity)),
        None => Arc::new(annotated),
    }
}

/// [`annotate_rhs`] at `Demand::Value` — a `Bind`/`Define`/`Source` RHS
/// bound to a name, or the toplevel's own tail `Run`.
fn annotate_value_rhs(rhs: &Arc<Comp>, ctx: &mut InferCtx, eta: bool) -> Arc<Comp> {
    annotate_rhs(rhs, ctx, eta, Demand::Value)
}

/// S3: rebuild an arrow-typed RHS of curried arity `arity` as
/// `Return(Thunk(λx₁. … λxₙ. App { head, args }))`, flattening `rhs` into
/// the `App`'s own head when it is itself an `App` — so every
/// function-typed thunk's body is a syntactic `Lam` ([`Comp::arrow`]).
fn eta_expand_arrow(rhs: Comp, ctx: &mut InferCtx, arity: usize) -> Comp {
    let span = rhs.span;
    let (head, mut args): (Arc<Comp>, Args) = match rhs.item {
        CompKind::App { head, args } => (head, args),
        other => (Arc::new(Spanned::with_span(span, other)), Vec::new()),
    };
    let params: Vec<String> = (0..arity).map(|_| ctx.fresh_name("eta")).collect();
    for param in &params {
        args.push(ValListElem::Single(Spanned::synthetic(Val::Variable(
            param.clone(),
        ))));
    }
    let body = params.into_iter().rev().fold(
        Spanned::with_span(span, CompKind::App { head, args }),
        |body, param| {
            Spanned::with_span(
                span,
                CompKind::Lam {
                    param: IrPattern::Name(param),
                    body: Arc::new(body),
                },
            )
        },
    );
    Spanned::with_span(span, CompKind::Return(Val::Thunk(Arc::new(body))))
}

fn annotate_demand(comp: &Comp, ctx: &mut InferCtx, eta: bool, demand: Demand) -> Comp {
    match &comp.item {
        // A `Wildcard` RHS is a discarded statement, `Demand::Discard`, but
        // still eta-expanded if it resolved to `Fun` (S3) — `annotate_rhs`
        // carries both.  Any other pattern's RHS is read at `Demand::Value`.
        // `Bind` never generalises a scheme, on either arm.
        CompKind::Bind {
            comp: rhs,
            pattern,
            rest,
        } => {
            let rhs_demand = if matches!(pattern.as_ref(), IrPattern::Wildcard) {
                Demand::Discard
            } else {
                Demand::Value
            };
            let item = CompKind::Bind {
                comp: annotate_rhs(rhs, ctx, eta, rhs_demand),
                pattern: Arc::new(annotate_pattern(pattern, ctx)),
                rest: Arc::new(annotate_demand(rest, ctx, eta, demand)),
            };
            return Spanned::with_span(comp.span, item);
        }
        CompKind::Source { path, rest } => {
            let item = CompKind::Source {
                path: annotate_rhs(path, ctx, eta, Demand::Value),
                rest: Arc::new(annotate_demand(rest, ctx, eta, demand)),
            };
            return Spanned::with_span(comp.span, item);
        }
        CompKind::Force(Val::Thunk(inner)) => {
            let item = CompKind::Force(Val::Thunk(Arc::new(annotate_demand(
                inner, ctx, eta, demand,
            ))));
            return Spanned::with_span(comp.span, item);
        }
        CompKind::If { cond, then, else_ } => {
            let item = CompKind::If {
                cond: annotate_spanned_val(cond, ctx),
                then: Arc::new(annotate_join_arm(comp, then, ctx, eta, demand)),
                else_: Arc::new(annotate_join_arm(comp, else_, ctx, eta, demand)),
            };
            return Spanned::with_span(comp.span, item);
        }
        CompKind::Case { scrutinee, arms } => {
            let item = CompKind::Case {
                scrutinee: annotate_spanned_val(scrutinee, ctx),
                arms: arms
                    .iter()
                    .map(|arm| CaseArm {
                        tag: arm.tag.clone(),
                        pattern: annotate_pattern(&arm.pattern, ctx),
                        body: arm.body.with_comp(Arc::new(annotate_join_arm(
                            comp,
                            arm.body.comp(),
                            ctx,
                            eta,
                            demand,
                        ))),
                    })
                    .collect(),
            };
            return Spanned::with_span(comp.span, item);
        }
        CompKind::Try { .. }
        | CompKind::Guard { .. }
        | CompKind::Within { .. }
        | CompKind::Grant { .. }
        | CompKind::Audit { .. }
        | CompKind::Redirect { .. } => return annotate_scope(comp, ctx, eta, demand),
        _ => {}
    }

    let item = annotate_plain(comp, ctx, eta);
    let wrapped = if demand == Demand::Value && bytes_result(ctx, comp_key(comp)) {
        captured_string(Spanned::with_span(comp.span, item), ctx)
    } else {
        item
    };
    Spanned::with_span(comp.span, wrapped)
}

/// The structural rebuild shared by every node `annotate_demand` doesn't walk
/// specially: every child is `Discard`.
fn annotate_plain(comp: &Comp, ctx: &mut InferCtx, eta: bool) -> CompKind {
    match &comp.item {
        CompKind::Pipeline {
            stages,
            stage_types,
            yields: _,
        } => {
            // The last place a route is read: a byte-routed pipeline's value
            // is `Unit` by WF-2, so nothing is worth shipping home from the
            // final stage's process.  Past here the IR is route-free.
            let yields = match ctx
                .pipeline_routes
                .get(&comp_key(comp))
                .copied()
                .map(|route| ctx.ground(route))
            {
                Some(GroundRoute::Bytes) => PipeYield::Unit,
                _ => PipeYield::Last,
            };
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
                    .map(|stage| Arc::new(annotate_demand(stage, ctx, eta, Demand::Discard)))
                    .collect(),
                stage_types,
                yields,
            }
        }
        CompKind::Lam { param, body } => CompKind::Lam {
            param: annotate_pattern(param, ctx),
            body: Arc::new(annotate_demand(body, ctx, eta, Demand::Discard)),
        },
        CompKind::App { head, args } => CompKind::App {
            head: Arc::new(annotate_demand(head, ctx, eta, Demand::Discard)),
            args: annotate_args(args, ctx),
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
            group: Arc::from(
                group
                    .iter()
                    .map(|(name, m)| {
                        (
                            name.clone(),
                            Arc::new(annotate_demand(m, ctx, eta, Demand::Discard)),
                        )
                    })
                    .collect::<Vec<_>>(),
            ),
            index: *index,
        },
        CompKind::Observe(reg) => CompKind::Observe(reg.clone()),
        // `Capture` and `Decode` are checker-inserted only, by this very pass;
        // the rest are walked directly by `annotate_demand` and never reach
        // here.
        CompKind::Capture(_)
        | CompKind::Decode(_)
        | CompKind::Bind { .. }
        | CompKind::Source { .. }
        | CompKind::If { .. }
        | CompKind::Case { .. }
        | CompKind::Try { .. }
        | CompKind::Guard { .. }
        | CompKind::Within { .. }
        | CompKind::Grant { .. }
        | CompKind::Audit { .. }
        | CompKind::Redirect { .. } => unreachable!("not a plain-rebuild node"),
    }
}

/// One arm of an `If`/`Case` join under `demand`: a byte-side join
/// walks a byte-payload arm at `Value` and wraps a subsumed (`∅`-`Unit`) arm
/// whole; otherwise `demand` simply inherits into the arm.
fn annotate_join_arm(join: &Comp, arm: &Comp, ctx: &mut InferCtx, eta: bool, demand: Demand) -> Comp {
    if byte_side_join(join, ctx, demand) {
        if bytes_result(ctx, comp_key(arm)) {
            return annotate_demand(arm, ctx, eta, Demand::Value);
        }
        return Spanned::with_span(
            arm.span,
            captured_string(annotate_demand(arm, ctx, eta, Demand::Discard), ctx),
        );
    }
    annotate_demand(arm, ctx, eta, demand)
}

/// A scope arm/body `Val`, dispatched the way [`annotate_join_arm`]
/// dispatches a `Comp` arm: `join`'s result decides byte-side-or-not, `val`'s
/// own recorded result (by its `Val` address) decides descend-vs-wrap.
fn annotate_scope_val(
    join: &Comp,
    val: &Val,
    ctx: &mut InferCtx,
    handler: bool,
    demand: Demand,
) -> Val {
    let walk = if !byte_side_join(join, ctx, demand) {
        ArmWalk::Plain
    } else if bytes_val_result(ctx, val_key(val)) {
        ArmWalk::Descend
    } else {
        ArmWalk::Wrap
    };
    match walk {
        ArmWalk::Plain => annotate_val(val, ctx),
        ArmWalk::Descend | ArmWalk::Wrap => match val {
            Val::Thunk(inner) if !handler => Val::Thunk(arm_body(inner, ctx, walk)),
            Val::Thunk(inner) => match &inner.item {
                CompKind::Lam { param, body } => Val::Thunk(Arc::new(Spanned::with_span(
                    inner.span,
                    CompKind::Lam {
                        param: annotate_pattern(param, ctx),
                        body: arm_body(body, ctx, walk),
                    },
                ))),
                _ => eta_expand_captured(val, ctx, handler),
            },
            _ => eta_expand_captured(val, ctx, handler),
        },
    }
}

/// A syntactic arm/handler body under [`ArmWalk::Descend`] (push `Value` in)
/// or [`ArmWalk::Wrap`] (wrap the whole body at `Discard`).
fn arm_body(body: &Arc<Comp>, ctx: &mut InferCtx, walk: ArmWalk) -> Arc<Comp> {
    match walk {
        ArmWalk::Descend => Arc::new(annotate_demand(body, ctx, false, Demand::Value)),
        ArmWalk::Wrap => Arc::new(Spanned::with_span(
            body.span,
            captured_string(annotate_demand(body, ctx, false, Demand::Discard), ctx),
        )),
        ArmWalk::Plain => unreachable!("arm_body is only called under Descend/Wrap"),
    }
}

/// [`captured_string`] around `force <val>`, thunked — or around
/// `force <val> e` under a fresh binder, for a handler. Safe: a scope forces
/// its arm exactly once and never returns it.
fn eta_expand_captured(val: &Val, ctx: &mut InferCtx, handler: bool) -> Val {
    let forced = Spanned::synthetic(CompKind::Force(annotate_val(val, ctx)));
    if !handler {
        let captured = Spanned::synthetic(captured_string(forced, ctx));
        return Val::Thunk(Arc::new(captured));
    }
    let param = "__capture_e".to_string();
    let app = Spanned::synthetic(CompKind::App {
        head: Arc::new(forced),
        args: vec![ValListElem::Single(Spanned::synthetic(Val::Variable(
            param.clone(),
        )))],
    });
    let captured = Spanned::synthetic(captured_string(app, ctx));
    Val::Thunk(Arc::new(Spanned::synthetic(CompKind::Lam {
        param: IrPattern::Name(param),
        body: Arc::new(captured),
    })))
}

fn annotate_val(val: &Val, ctx: &mut InferCtx) -> Val {
    match val {
        Val::Thunk(comp) => Val::Thunk(Arc::new(annotate(comp, ctx))),
        Val::List(elems) => Val::List(elems.iter().map(|e| annotate_list_elem(e, ctx)).collect()),
        Val::Map(entries) => Val::Map(
            entries
                .iter()
                .map(|e| match e {
                    ValMapEntry::Entry(k, v) => {
                        ValMapEntry::Entry(annotate_val(k, ctx), annotate_spanned_val(v, ctx))
                    }
                    ValMapEntry::Spread(v) => {
                        ValMapEntry::Spread(annotate_spanned_val(v, ctx))
                    }
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
        | Val::Variable(_) => val.clone(),
    }
}

fn annotate_spanned_val(value: &Spanned<Val>, ctx: &mut InferCtx) -> Spanned<Val> {
    Spanned::with_span(value.span, annotate_val(&value.item, ctx))
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

fn annotate_scope(comp: &Comp, ctx: &mut InferCtx, eta: bool, demand: Demand) -> Comp {
    let item = match &comp.item {
        CompKind::Try { body, handler } => CompKind::Try {
            body: annotate_scope_val(comp, body, ctx, false, demand),
            handler: annotate_scope_val(comp, handler, ctx, true, demand),
        },
        CompKind::Guard { body, cleanup } => CompKind::Guard {
            body: annotate_scope_val(comp, body, ctx, false, demand),
            cleanup: annotate_val(cleanup, ctx),
        },
        CompKind::Within { opts, body } => CompKind::Within {
            opts: annotate_val(opts, ctx),
            body: annotate_scope_val(comp, body, ctx, false, demand),
        },
        CompKind::Grant { caps, body } => CompKind::Grant {
            caps: annotate_val(caps, ctx),
            body: annotate_scope_val(comp, body, ctx, false, demand),
        },
        CompKind::Audit { body } => CompKind::Audit {
            body: annotate_val(body, ctx),
        },
        CompKind::Redirect { body, redirects } => CompKind::Redirect {
            body: Arc::new(annotate_demand(body, ctx, eta, demand)),
            redirects: redirects
                .iter()
                .map(|r| annotate_redirect(r, ctx))
                .collect(),
        },
        _ => unreachable!("annotate_scope called on a non-scope node"),
    };
    Spanned::with_span(comp.span, item)
}

/// Map-pattern defaults are the only `Comp` a pattern carries.
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
                        .map(|d| Arc::new(annotate(d, ctx))),
                })
                .collect(),
        ),
    }
}

/// Rebuild a checked [`Toplevel`]: every phrase's RHS is walked at `eta =
/// true`, so S3's η-expansion applies throughout — a `Define`'s RHS and a
/// `Source`'s path are read at `Value` demand; every `Run`, tail included,
/// is `Demand::Discard` — a `Run`'s bytes are never captured into its own
/// reported value, only its arrow arity read for η-expansion.  `schemes`,
/// parallel to `top.phrases`, is
/// [`infer::infer_toplevel`](super::infer::infer_toplevel)'s per-`Define`
/// harvest, written straight onto the rebuilt `Phrase::Define` — `Bind`
/// never carries a scheme, on any path.
pub(super) fn annotate_toplevel(
    top: &Toplevel,
    ctx: &mut InferCtx,
    schemes: Vec<Vec<(String, Scheme)>>,
) -> Toplevel {
    let tail_index = top.phrases.len().saturating_sub(1);
    let phrases = top
        .phrases
        .iter()
        .zip(schemes)
        .enumerate()
        .map(|(index, (phrase, names))| {
            let item = match &phrase.item {
                Phrase::Define { pattern, comp, .. } => Phrase::Define {
                    pattern: Arc::new(annotate_pattern(pattern, ctx)),
                    comp: annotate_value_rhs(comp, ctx, true),
                    schemes: names
                        .into_iter()
                        .map(|(name, scheme)| (name, Arc::new(scheme)))
                        .collect(),
                },
                Phrase::Source { path } => Phrase::Source {
                    path: annotate_value_rhs(path, ctx, true),
                },
                // The tail's value is reported (η-expanded if it resolved
                // to `Fun`), but never byte-captured: nothing downstream
                // decodes the run's own report as text, so `Demand::Discard`
                // throughout — matching `infer_phrase`'s `force_discarded_shape`.
                Phrase::Run(comp) if index == tail_index => {
                    Phrase::Run(annotate_rhs(comp, ctx, true, Demand::Discard))
                }
                Phrase::Run(comp) => {
                    Phrase::Run(Arc::new(annotate_demand(comp, ctx, true, Demand::Discard)))
                }
            };
            Spanned::with_span(phrase.span, item)
        })
        .collect();
    Toplevel { phrases }
}
