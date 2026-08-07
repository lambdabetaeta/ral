//! Write-back pass: rebuild a checked comp with the inferencer's verdicts —
//! generalised schemes, ground byte modes, pipeline wires, `Capture` nodes —
//! over the tree that was inferred, using [`InferCtx`]'s node-address-keyed
//! side maps.
//!
//! `Capture` insertion is one recursive walk carrying a [`Demand`]: `Value`
//! where a boundary reads a payload, `Discard` where one is dropped. `Value`
//! reaching a node whose recorded `result` grounds `Bytes` wraps it in
//! `Capture`; the demand follows the same path the payload rides at run
//! time, so the wrap lands at the leaf that actually owns the bytes.

use super::env::{InferCtx, TyEnv};
use super::generalize::generalize;
use crate::ir::{
    Comp, CompKind, Exec, IrPattern, RedirectV, ScopeOp, Val, ValListElem, ValMapEntry,
    ValRedirectTarget,
};
use crate::mode::{ByteMode, Wire};
use crate::source::Spanned;
use crate::syntax::ast::MapPatternEntry;
use crate::syntax::tag::is_tag_label;
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
/// both read `Empty`, via [`InferCtx::ground`].
fn bytes_result(ctx: &mut InferCtx, key: usize) -> bool {
    matches!(
        ctx.results.get(&key).copied().map(|m| ctx.ground(m)),
        Some(ByteMode::Bytes)
    )
}

/// The `Val`-keyed analogue of [`bytes_result`], for scope arms.
fn bytes_val_result(ctx: &mut InferCtx, key: usize) -> bool {
    matches!(
        ctx.val_results.get(&key).copied().map(|m| ctx.ground(m)),
        Some(ByteMode::Bytes)
    )
}

/// A `Bind`'s scheme (spine only) and its RHS, always walked at `Value`
/// demand — a bind's whole point is to observe its RHS.
fn annotate_bind(
    comp: &Comp,
    rhs: &Arc<Comp>,
    ctx: &mut InferCtx,
    spine: bool,
) -> (Arc<Comp>, Option<Box<crate::typecheck::Scheme>>) {
    let scheme = spine
        .then(|| ctx.bind_tys.get(&comp_key(comp)).cloned())
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
    let rhs = Arc::new(annotate_demand(rhs, ctx, false, Demand::Value));
    (rhs, scheme)
}

/// Rebuild `comp` under `demand`; `spine` marks the `Bind`s that install into
/// the persistent session scope, so only those carry a generalised scheme
/// (closed against an empty environment, since the unifier dies with the run).
pub(super) fn annotate(comp: &Comp, ctx: &mut InferCtx, spine: bool) -> Comp {
    annotate_demand(comp, ctx, spine, Demand::Discard)
}

fn annotate_demand(comp: &Comp, ctx: &mut InferCtx, spine: bool, demand: Demand) -> Comp {
    match &comp.item {
        CompKind::Seq(parts) => {
            let item = match parts.split_last() {
                None => CompKind::Seq(Vec::new()),
                Some((last, init)) => {
                    let mut rebuilt: Vec<Arc<Comp>> = init
                        .iter()
                        .map(|p| Arc::new(annotate_demand(p, ctx, spine, Demand::Discard)))
                        .collect();
                    rebuilt.push(Arc::new(annotate_demand(last, ctx, spine, demand)));
                    CompKind::Seq(rebuilt)
                }
            };
            return Spanned::with_span(comp.span, item);
        }
        CompKind::Bind {
            comp: rhs,
            pattern,
            rest,
            ..
        } => {
            let (rhs, scheme) = annotate_bind(comp, rhs, ctx, spine);
            let item = CompKind::Bind {
                comp: rhs,
                pattern: annotate_pattern(pattern, ctx),
                rest: Arc::new(annotate_demand(rest, ctx, spine, demand)),
                scheme,
            };
            return Spanned::with_span(comp.span, item);
        }
        CompKind::Force(Val::Thunk(inner)) => {
            let item = CompKind::Force(Val::Thunk(Arc::new(annotate_demand(
                inner, ctx, false, demand,
            ))));
            return Spanned::with_span(comp.span, item);
        }
        CompKind::If { cond, then, else_ } => {
            let item = CompKind::If {
                cond: annotate_spanned_val(cond, ctx),
                then: Arc::new(annotate_join_arm(comp, then, ctx, demand)),
                else_: Arc::new(annotate_join_arm(comp, else_, ctx, demand)),
            };
            return Spanned::with_span(comp.span, item);
        }
        CompKind::Chain(parts) => {
            let item = CompKind::Chain(
                parts
                    .iter()
                    .map(|p| Arc::new(annotate_join_arm(comp, p, ctx, demand)))
                    .collect(),
            );
            return Spanned::with_span(comp.span, item);
        }
        CompKind::Case { scrutinee, table } => {
            let item = CompKind::Case {
                scrutinee: annotate_spanned_val(scrutinee, ctx),
                table: Spanned::with_span(
                    table.span,
                    annotate_case_table(comp, &table.item, ctx, demand),
                ),
            };
            return Spanned::with_span(comp.span, item);
        }
        CompKind::Scope(op) => return annotate_scope(comp, op, ctx, demand),
        _ => {}
    }

    let item = annotate_plain(comp, ctx);
    let wrapped = if demand == Demand::Value && bytes_result(ctx, comp_key(comp)) {
        CompKind::Capture(Arc::new(Spanned::with_span(comp.span, item)))
    } else {
        item
    };
    Spanned::with_span(comp.span, wrapped)
}

/// The structural rebuild shared by every node `annotate_demand` doesn't walk
/// specially: every child is `Discard`.
fn annotate_plain(comp: &Comp, ctx: &mut InferCtx) -> CompKind {
    match &comp.item {
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
                        .get(&comp_key(stage))
                        .copied()
                        .map_or(*placeholder, |spec| Wire {
                            input: ctx.ground(spec.input),
                            output: ctx.ground(spec.output),
                        })
                })
                .collect();
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
        CompKind::LetRec { slot, bindings } => CompKind::LetRec {
            slot: *slot,
            bindings: Arc::new(
                bindings
                    .iter()
                    .map(|(name, value)| (name.clone(), annotate_val(value, ctx)))
                    .collect(),
            ),
        },
        // `Capture` is checker-inserted only, by this very pass; the rest are
        // walked directly by `annotate_demand` and never reach here.
        CompKind::Capture(_)
        | CompKind::Seq(_)
        | CompKind::Bind { .. }
        | CompKind::If { .. }
        | CompKind::Chain(_)
        | CompKind::Case { .. }
        | CompKind::Scope(_) => unreachable!("not a plain-rebuild node"),
    }
}

/// One arm of an `If`/`Chain` join under `demand`: a byte-side join walks a
/// byte-payload arm at `Value` and wraps a subsumed (`∅`-`Unit`) arm whole;
/// otherwise `demand` simply inherits into the arm.
fn annotate_join_arm(join: &Comp, arm: &Comp, ctx: &mut InferCtx, demand: Demand) -> Comp {
    if demand == Demand::Value && bytes_result(ctx, comp_key(join)) {
        if bytes_result(ctx, comp_key(arm)) {
            return annotate_demand(arm, ctx, false, Demand::Value);
        }
        return Spanned::with_span(
            arm.span,
            CompKind::Capture(Arc::new(annotate_demand(arm, ctx, false, Demand::Discard))),
        );
    }
    annotate_demand(arm, ctx, false, demand)
}

/// A `case` handler table: every literal tag entry is a join arm, dispatched
/// like a `try` handler — the `case` node's own result decides byte-side-or-
/// not, each handler's decides descend-vs-wrap.  A table that is not a literal
/// map (it came from a parameter) has no arms to reach here and rebuilds flat.
fn annotate_case_table(join: &Comp, table: &Val, ctx: &mut InferCtx, demand: Demand) -> Val {
    let Val::Map(entries) = table else {
        return annotate_val(table, ctx);
    };
    Val::Map(
        entries
            .iter()
            .map(|entry| match entry {
                ValMapEntry::Entry(key @ Val::String(label), value @ Val::Thunk(_))
                    if is_tag_label(label) =>
                {
                    ValMapEntry::Entry(
                        annotate_val(key, ctx),
                        annotate_scope_val(join, value, ctx, true, demand),
                    )
                }
                ValMapEntry::Entry(key, value) => {
                    ValMapEntry::Entry(annotate_val(key, ctx), annotate_val(value, ctx))
                }
                ValMapEntry::Spread(val) => ValMapEntry::Spread(annotate_val(val, ctx)),
            })
            .collect(),
    )
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
    let walk = if demand != Demand::Value || !bytes_result(ctx, comp_key(join)) {
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
            CompKind::Capture(Arc::new(annotate_demand(body, ctx, false, Demand::Discard))),
        )),
        ArmWalk::Plain => unreachable!("arm_body is only called under Descend/Wrap"),
    }
}

/// `{ capture (force <val>) }`, or `{ |e| capture (force <val> e) }` for a
/// handler. Safe: a scope forces its arm exactly once and never returns it.
fn eta_expand_captured(val: &Val, ctx: &mut InferCtx, handler: bool) -> Val {
    let forced = Spanned::synthetic(CompKind::Force(annotate_val(val, ctx)));
    if !handler {
        let captured = Spanned::synthetic(CompKind::Capture(Arc::new(forced)));
        return Val::Thunk(Arc::new(captured));
    }
    let param = "__capture_e".to_string();
    let app = Spanned::synthetic(CompKind::App {
        head: Arc::new(forced),
        args: vec![Spanned::synthetic(ValListElem::Single(Val::Variable(
            param.clone(),
        )))],
    });
    let captured = Spanned::synthetic(CompKind::Capture(Arc::new(app)));
    Val::Thunk(Arc::new(Spanned::synthetic(CompKind::Lam {
        param: IrPattern::Name(param),
        body: Arc::new(captured),
    })))
}

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

fn annotate_spanned_val(value: &Spanned<Val>, ctx: &mut InferCtx) -> Spanned<Val> {
    Spanned::with_span(value.span, annotate_val(&value.item, ctx))
}

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

fn annotate_scope(comp: &Comp, op: &ScopeOp, ctx: &mut InferCtx, demand: Demand) -> Comp {
    let item = match op {
        ScopeOp::Try { body, handler } => CompKind::Scope(ScopeOp::Try {
            body: annotate_scope_val(comp, body, ctx, false, demand),
            handler: annotate_scope_val(comp, handler, ctx, true, demand),
        }),
        ScopeOp::Guard { body, cleanup } => CompKind::Scope(ScopeOp::Guard {
            body: annotate_scope_val(comp, body, ctx, false, demand),
            cleanup: annotate_val(cleanup, ctx),
        }),
        ScopeOp::Within { opts, body } => CompKind::Scope(ScopeOp::Within {
            opts: annotate_val(opts, ctx),
            body: annotate_scope_val(comp, body, ctx, false, demand),
        }),
        ScopeOp::Grant { caps, body } => CompKind::Scope(ScopeOp::Grant {
            caps: annotate_val(caps, ctx),
            body: annotate_scope_val(comp, body, ctx, false, demand),
        }),
        ScopeOp::Audit { body } => CompKind::Scope(ScopeOp::Audit {
            body: annotate_val(body, ctx),
        }),
        ScopeOp::Redirect { body, redirects } => CompKind::Scope(ScopeOp::Redirect {
            body: Arc::new(annotate_demand(body, ctx, false, demand)),
            redirects: redirects
                .iter()
                .map(|r| annotate_redirect(r, ctx))
                .collect(),
        }),
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
                        .map(|d| Arc::new(annotate(d, ctx, false))),
                })
                .collect(),
        ),
    }
}
