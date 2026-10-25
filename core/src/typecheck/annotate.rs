//! Write-back pass: rebuild a checked comp with the inferencer's verdicts —
//! generalised schemes, boundary sites, and `capture` coercions — over the
//! tree that was inferred, using [`InferCtx`]'s node-address-keyed side maps.
//!
//! The walk is a plain structural rebuild: the commands inference recorded
//! as captured are wrapped in [`captured_string`] where they stand, and a
//! captured value in hand is η-wrapped into a block that captures its call.
//! A right-hand side of function type is bound unrun, as its thunk. Its state
//! is [`Annotate`], the context.

use super::env::{InferCtx, Node};
use crate::ir::Redirects;
use crate::ir::{
    Args, Assembly, CaseArm, CommandName, CommandWord, Comp, CompKind, DefineSchemes, Exec,
    GroupNode, HandlerArmV, Name, OptionsV, Pattern, Phrase, Toplevel, Val, ValListElem,
    ValMapEntry, ValRecordEntry,
};
use crate::source::{Span, Spanned};
use crate::ty::Site;
use std::sync::Arc;

/// The write-back walk.
struct Annotate<'a> {
    ctx: &'a mut InferCtx,
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
    let name = ctx.fresh_name("decode");
    CompKind::bind(
        Pattern::Name(name.clone()),
        Spanned::with_span(span, CompKind::Capture(Arc::new(body))),
        Spanned::with_span(span, CompKind::Decode(Val::Variable(name))),
    )
}

/// `n` fresh parameters and the arguments that pass them on.
fn eta(ctx: &mut InferCtx, n: usize) -> (Vec<Name>, Args) {
    let params: Vec<Name> = (0..n).map(|_| ctx.fresh_name("eta")).collect();
    let args = params
        .iter()
        .map(|p| ValListElem::Single(Spanned::synthetic(Val::Variable(p.clone()))))
        .collect();
    (params, args)
}

/// `{ |x₁…xₙ| cap (!v x₁ … xₙ) to d. decode d }`: a captured value in hand,
/// its call wrapped where a literal block's body would be.
fn captured_block(value: Val, arity: usize, span: Option<Span>, ctx: &mut InferCtx) -> Val {
    let (params, args) = eta(ctx, arity);
    let forced = Spanned::with_span(span, CompKind::Force(value));
    let call = if params.is_empty() {
        forced.item
    } else {
        CompKind::App {
            head: Arc::new(forced),
            args,
        }
    };
    let body = captured_string(Spanned::with_span(span, call), ctx);
    lambda(span, params, body)
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
                    param: Pattern::Name(param),
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
    let (params, args) = eta(ctx, arity);
    let call = CompKind::Exec(Exec {
        head: CommandWord::Name(CommandName::Bare(name.into())),
        args,
        redirects: Redirects::default(),
        site: Some(site),
    });
    lambda(span, params, call)
}

impl Annotate<'_> {
    /// Rebuild `group`, memoized by its source `Arc`'s identity: every `Rec`
    /// projection of one group is walked here once, so all of them keep sharing
    /// one rebuilt node, as the elaborator built them.
    fn rec_group(&mut self, group: &Arc<GroupNode>) -> Arc<GroupNode> {
        let key = Node::group(group);
        if let Some(rebuilt) = self.ctx.rec_group_rebuilds.get(&key) {
            return Arc::clone(rebuilt);
        }
        let rebuilt = GroupNode::new(
            group
                .shape()
                .iter()
                .map(|(name, m)| (name.clone(), Arc::new(self.comp(m))))
                .collect::<Vec<_>>()
                .into(),
        );
        self.ctx
            .rec_group_rebuilds
            .insert(key, Arc::clone(&rebuilt));
        rebuilt
    }

    /// Rebuild the right-hand side of a `Bind` or `Define`; one of function
    /// type is bound unrun, as `return (thunk M)` (Let-fun).  `Bind` never
    /// generalises a scheme — that lives on `Phrase::Define` alone, one per
    /// bound name.
    fn rhs(&mut self, rhs: &Arc<Comp>) -> Arc<Comp> {
        let annotated = self.comp(rhs);
        Arc::new(if self.ctx.thunked.contains(&Node::comp(rhs)) {
            Spanned::with_span(rhs.span, CompKind::Return(Val::thunk(Arc::new(annotated))))
        } else {
            annotated
        })
    }

    fn comp(&mut self, comp: &Comp) -> Comp {
        let item = match &comp.item {
            CompKind::Bind {
                comp: rhs,
                pattern,
                rest,
            } => CompKind::Bind {
                comp: self.rhs(rhs),
                pattern: Arc::clone(pattern),
                rest: Arc::new(self.comp(rest)),
            },
            CompKind::Pipeline { stages } => CompKind::Pipeline {
                stages: stages
                    .iter()
                    .map(|stage| Arc::new(self.comp(stage)))
                    .collect(),
            },
            CompKind::Lam { param, body } => CompKind::Lam {
                param: param.clone(),
                body: Arc::new(self.comp(body)),
            },
            CompKind::App { head, args } => CompKind::App {
                head: Arc::new(self.comp(head)),
                args: self.args(args),
            },
            CompKind::Protect { body, site } => CompKind::Protect {
                body: Arc::new(self.comp(body)),
                site: Arc::clone(site),
            },
            CompKind::Force(value) => CompKind::Force(self.val(value)),
            CompKind::Return(value) => CompKind::Return(self.val(value)),
            CompKind::Assemble(assembly) => CompKind::Assemble(self.assembly(assembly)),
            CompKind::Exec(e) => self.exec(comp, e),
            CompKind::Binary(op, lhs, rhs) => CompKind::Binary(*op, self.val(lhs), self.val(rhs)),
            CompKind::Negate(value) => CompKind::Negate(self.val(value)),
            CompKind::Not(value) => CompKind::Not(self.val(value)),
            CompKind::Index { target, keys } => CompKind::Index {
                target: self.val(target),
                keys: keys.iter().map(|k| self.spanned_val(k)).collect(),
            },
            CompKind::Interpolation(parts) => {
                CompKind::Interpolation(parts.iter().map(|v| self.val(v)).collect())
            }
            CompKind::Rec { group, index } => CompKind::Rec {
                group: self.rec_group(group),
                index: *index,
            },
            CompKind::Tilde(path) => CompKind::Tilde(path.clone()),
            CompKind::If { cond, then, else_ } => CompKind::If {
                cond: self.spanned_val(cond),
                then: self.spanned_val(then),
                else_: self.spanned_val(else_),
            },
            CompKind::Case { scrutinee, arms } => CompKind::Case {
                scrutinee: self.spanned_val(scrutinee),
                arms: arms
                    .iter()
                    .map(|arm| CaseArm {
                        tag: arm.tag.clone(),
                        body: self.spanned_val(&arm.body),
                    })
                    .collect(),
            },
            CompKind::Try { body, handler } => CompKind::Try {
                body: self.val(body),
                handler: self.val(handler),
            },
            CompKind::Guard { body, cleanup } => CompKind::Guard {
                body: self.val(body),
                cleanup: self.val(cleanup),
            },
            CompKind::Within {
                opts,
                handlers,
                body,
            } => CompKind::Within {
                opts: self.options(opts),
                handlers: handlers.as_ref().map(|arms| {
                    arms.iter()
                        .map(|arm| HandlerArmV {
                            name: arm.name.clone(),
                            value: self.spanned_val(&arm.value),
                        })
                        .collect()
                }),
                body: self.val(body),
            },
            CompKind::Grant { caps, body } => CompKind::Grant {
                caps: self.options(caps),
                body: self.val(body),
            },
            CompKind::Audit { body } => CompKind::Audit {
                body: self.val(body),
            },
            CompKind::Spawn { title, body } => CompKind::Spawn {
                title: title.as_ref().map(|t| self.spanned_val(t)),
                body: self.val(body),
            },
            CompKind::Gather { body } => CompKind::Gather {
                body: self.val(body),
            },
            CompKind::Redirect { body, redirects } => CompKind::Redirect {
                body: Arc::new(self.comp(body)),
                redirects: redirects.map(|v| self.val(v)),
            },
            CompKind::Capture(body) => CompKind::Capture(Arc::new(self.comp(body))),
            CompKind::Decode(value) => CompKind::Decode(self.val(value)),
        };
        let item = if self.ctx.captured.contains(&Node::comp(comp)) {
            captured_string(Spanned::with_span(comp.span, item), self.ctx)
        } else {
            item
        };
        Spanned::with_span(comp.span, item)
    }

    /// An `Exec`: its site from the checker, η-expanded when it is an
    /// under-applied boundary.
    fn exec(&mut self, comp: &Comp, e: &Exec) -> CompKind {
        if let Some(site) = self.ctx.imported_sites.get(&Node::comp(comp)).cloned() {
            let CommandWord::Name(CommandName::Bare(name)) = &e.head else {
                unreachable!("a stored binding has a bare name")
            };
            let force = Spanned::with_span(
                comp.span,
                CompKind::Force(Val::Protected {
                    value: Box::new(Val::Variable(name.clone())),
                    site,
                }),
            );
            let args = self.args(&e.args);
            let call = if args.is_empty() {
                force.item
            } else {
                CompKind::App {
                    head: Arc::new(force),
                    args,
                }
            };
            return if e.redirects.is_empty() {
                call
            } else {
                CompKind::Redirect {
                    body: Arc::new(Spanned::with_span(comp.span, call)),
                    redirects: e.redirects.map(|v| self.val(v)),
                }
            };
        }
        let mut exec = Exec {
            head: e.head.clone(),
            args: self.args(&e.args),
            redirects: e.redirects.map(|v| self.val(v)),
            site: self.ctx.sites.get(&Node::comp(comp)).cloned(),
        };
        // An under-applied boundary is a boundary held as a value: a block of the
        // saturated call, wherever it stands.
        match self.ctx.boundary_missing.get(&Node::comp(comp)).copied() {
            Some(missing) => {
                let (params, args) = eta(self.ctx, missing);
                exec.args.extend(args);
                lambda_comp(comp.span, params, CompKind::Exec(exec)).item
            }
            None => CompKind::Exec(exec),
        }
    }

    fn val(&mut self, val: &Val) -> Val {
        self.val_at(val, None)
    }

    /// [`Self::val`] for a value written at `span`, which the block a boundary
    /// reference or a captured value becomes is given, so its errors point where
    /// the value is.
    fn val_at(&mut self, val: &Val, span: Option<Span>) -> Val {
        if let Some(value) = self.ctx.boundary_values.get(&Node::val(val))
            && let Some(site) = self.ctx.sites.get(&Node::val(val)).cloned()
        {
            let (name, arity) = (value.name.clone(), value.arity);
            return boundary_block(&name, arity, site, span, self.ctx);
        }
        let rebuilt = self.rebuild_val(val);
        // Pure data was admitted whole by `readmits`; only delayed parts need a membrane.
        let rebuilt = match self.ctx.imported_sites.get(&Node::val(val)) {
            Some(site) if site.has_delayed() => Val::Protected {
                value: Box::new(rebuilt),
                site: Arc::clone(site),
            },
            _ => rebuilt,
        };
        match self.ctx.captured_vals.get(&Node::val(val)).copied() {
            Some(arity) => captured_block(rebuilt, arity, span, self.ctx),
            None => rebuilt,
        }
    }

    /// The structural rebuild of a value.
    fn rebuild_val(&mut self, val: &Val) -> Val {
        match val {
            Val::Protected { value, site } => Val::Protected {
                value: Box::new(self.val(value)),
                site: Arc::clone(site),
            },
            Val::Thunk(comp) => Val::thunk(Arc::new(self.comp(comp.shape()))),
            Val::List(elems) => Val::list(
                elems
                    .shape()
                    .iter()
                    .map(|v| self.spanned_val(v))
                    .collect::<Vec<_>>(),
            ),
            Val::Record(entries) => Val::record(
                entries
                    .shape()
                    .iter()
                    .map(|(k, v)| (k.clone(), self.spanned_val(v)))
                    .collect::<Vec<_>>(),
            ),
            Val::Map(entries) => Val::map(
                entries
                    .shape()
                    .iter()
                    .map(|(k, v)| (k.clone(), self.spanned_val(v)))
                    .collect::<Vec<_>>(),
            ),
            Val::Variant { label, payload } => Val::Variant {
                label: label.clone(),
                payload: payload.as_ref().map(|p| Box::new(self.val(p))),
            },
            Val::Unit
            | Val::String(_)
            | Val::Int(_)
            | Val::Float(_)
            | Val::Bool(_)
            | Val::Variable(_) => val.clone(),
        }
    }

    fn spanned_val(&mut self, value: &Spanned<Val>) -> Spanned<Val> {
        Spanned::with_span(value.span, self.val_at(&value.item, value.span))
    }

    fn list_elem(&mut self, elem: &ValListElem) -> ValListElem {
        match elem {
            ValListElem::Single(v) => ValListElem::Single(self.spanned_val(v)),
            ValListElem::Spread(v) => ValListElem::Spread(self.spanned_val(v)),
        }
    }

    fn elems(&mut self, elems: &[ValListElem]) -> Vec<ValListElem> {
        elems.iter().map(|e| self.list_elem(e)).collect()
    }

    fn args(&mut self, args: &Args) -> Args {
        self.elems(args).into()
    }

    fn assembly(&mut self, assembly: &Assembly) -> Assembly {
        match assembly {
            Assembly::List(elems) => Assembly::List(self.elems(elems)),
            Assembly::Record(entries) => Assembly::Record(
                entries
                    .iter()
                    .map(|e| match e {
                        ValRecordEntry::Field(label, v) => {
                            ValRecordEntry::Field(label.clone(), self.spanned_val(v))
                        }
                        ValRecordEntry::Spread(v) => ValRecordEntry::Spread(self.spanned_val(v)),
                    })
                    .collect(),
            ),
            Assembly::Map(entries) => Assembly::Map(
                entries
                    .iter()
                    .map(|e| match e {
                        ValMapEntry::Entry(k, v) => {
                            ValMapEntry::Entry(self.val(k), self.spanned_val(v))
                        }
                        ValMapEntry::Spread(v) => ValMapEntry::Spread(self.spanned_val(v)),
                    })
                    .collect(),
            ),
        }
    }

    /// A form's written options, each value annotated where it stands.
    fn options(&mut self, opts: &OptionsV) -> OptionsV {
        opts.iter()
            .map(|(name, value)| {
                let value = Spanned::with_span(value.span, self.val(&value.item));
                (name.clone(), value)
            })
            .collect()
    }
}

/// Rebuild a checked [`Toplevel`]: a `Define`'s RHS is rebuilt as any `let`'s
/// is.  A `Run` is a statement, held ready to run, so nothing thunks
/// it and nothing captures it.  `schemes`, parallel to `phrases`, is
/// [`infer::infer_toplevel`](super::infer::infer_toplevel)'s per-`Define`
/// harvest, written straight onto the rebuilt `Phrase::Define` — `Bind`
/// never carries a scheme, on any path.
pub(super) fn annotate_toplevel(
    phrases: &[Spanned<Phrase<()>>],
    ctx: &mut InferCtx,
    schemes: Vec<DefineSchemes>,
) -> Toplevel {
    let mut walk = Annotate { ctx };
    let phrases = phrases
        .iter()
        .zip(schemes)
        .map(|(phrase, names)| {
            let item = match &phrase.item {
                Phrase::Define { pattern, comp, .. } => Phrase::Define {
                    pattern: Arc::clone(pattern),
                    comp: walk.rhs(comp),
                    schemes: names,
                },
                Phrase::Run(comp) => Phrase::Run(Arc::new(walk.comp(comp))),
            };
            Spanned::with_span(phrase.span, item)
        })
        .collect();
    Toplevel {
        phrases,
        admits: walk.ctx.readmits.clone(),
    }
}
