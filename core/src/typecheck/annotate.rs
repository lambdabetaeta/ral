//! Write-back pass: rebuild a checked comp with the inferencer's verdicts —
//! generalised schemes, boundary sites, and grade coercions — over the
//! tree that was inferred, using [`InferCtx`]'s node-address-keyed side maps.
//!
//! The walk is a plain structural rebuild: the commands inference recorded
//! as captured are wrapped in [`captured_string`] where they stand, and a
//! captured value in hand is η-wrapped into a block that captures its call.
//! A right-hand side of function type is bound unrun, as its thunk, and a
//! call whose head returned a thunk is split to force it ([`Annotate::split`]).
//! Its state is [`Annotate`], the context.

use super::env::{InferCtx, Node};
use super::grade::Adaptation;
use crate::ir::Redirects;
use crate::ir::{
    Argv, CaseArm, CommandName, CommandWord, Comp, CompKind, DefineSchemes, Exec, FieldsNode,
    GroupNode, HandlerArmV, Name, Pattern, Phrase, Prim, Toplevel, Val, ValListElem,
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
        Spanned::with_span(span, CompKind::Prim(Prim::Decode(Val::Variable(name)))),
    )
}

/// `n` fresh parameters and the arguments that pass them on.
fn eta(ctx: &mut InferCtx, n: usize) -> (Vec<Name>, Vec<Spanned<Val>>) {
    let params: Vec<Name> = (0..n).map(|_| ctx.fresh_name("eta")).collect();
    let args = params
        .iter()
        .map(|p| Spanned::synthetic(Val::Variable(p.clone())))
        .collect();
    (params, args)
}

/// `head` applied to `args`; with none, the head alone.
fn applied(span: Option<Span>, head: Comp, args: Vec<Spanned<Val>>) -> Comp {
    if args.is_empty() {
        head
    } else {
        Spanned::with_span(
            span,
            CompKind::App {
                head: Arc::new(head),
                args: args.into(),
            },
        )
    }
}

/// `body` under `redirects`, when there are any.
fn redirected(body: Comp, redirects: Redirects<Val>) -> CompKind {
    if redirects.is_empty() {
        body.item
    } else {
        CompKind::Redirect {
            body: Arc::new(body),
            redirects,
        }
    }
}

/// Adapt a value in hand under exactly `arity` fresh lambdas.
fn adapted_block(
    value: Val,
    arity: usize,
    how: Adaptation,
    span: Option<Span>,
    ctx: &mut InferCtx,
) -> Val {
    let (params, args) = eta(ctx, arity);
    let forced = Spanned::with_span(span, CompKind::Force(value));
    let call = applied(span, forced, args);
    let body = match how {
        Adaptation::Capture => captured_string(call, ctx),
        Adaptation::Upcast => CompKind::Upcast(Arc::new(call)),
    };
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
                    param,
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
        argv: args.into_iter().map(ValListElem::Single).collect(),
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
            CompKind::Seq { first, rest } => CompKind::Seq {
                first: Arc::new(self.comp(first)),
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
            CompKind::App { head, args } => {
                let head = self.comp(head);
                let args = self.args(args);
                self.app(comp, head, args)
            }
            CompKind::Protect { body, site } => CompKind::Protect {
                body: Arc::new(self.comp(body)),
                site: Arc::clone(site),
            },
            CompKind::Force(value) => CompKind::Force(self.val(value)),
            CompKind::Return(value) => CompKind::Return(self.val(value)),
            CompKind::Exec(e) => self.exec(comp, e),
            CompKind::Native { .. } => {
                unreachable!("a native's code is built by forcing it, never checked")
            }
            CompKind::Prim(prim) => {
                CompKind::Prim(prim.map_operands(|value, span| self.val_at(value, span)))
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
                opts: self.fields(opts),
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
                caps: self.fields(caps),
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
            CompKind::Upcast(body) => CompKind::Upcast(Arc::new(self.comp(body))),
        };
        let item = match self.ctx.adapted.get(&Node::comp(comp)) {
            Some(Adaptation::Capture) => {
                captured_string(Spanned::with_span(comp.span, item), self.ctx)
            }
            Some(Adaptation::Upcast) => {
                CompKind::Upcast(Arc::new(Spanned::with_span(comp.span, item)))
            }
            None => item,
        };
        Spanned::with_span(comp.span, item)
    }

    /// `head` applied to `args`, the call at `node`, split where the checker
    /// forced a returned thunk.
    fn app(&mut self, node: &Comp, head: Comp, args: Vec<Spanned<Val>>) -> CompKind {
        match self.ctx.splits.get(&Node::comp(node)).cloned() {
            Some(splits) => self.split(node.span, &splits, args, |args| {
                applied(node.span, head, args)
            }),
            None => applied(node.span, head, args).item,
        }
    }

    /// A call whose head returned thunks the checker forced before the
    /// arguments at `splits`: `first` takes the arguments before the
    /// first position, and each later segment applies the thunk the one
    /// before returned, `first to h₁. force h₁ a[k₁..k₂] to h₂. … force hₘ
    /// a[kₘ..]`.
    fn split(
        &mut self,
        span: Option<Span>,
        splits: &[usize],
        mut args: Vec<Spanned<Val>>,
        first: impl FnOnce(Vec<Spanned<Val>>) -> Comp,
    ) -> CompKind {
        let mut later: Vec<_> = splits.iter().rev().map(|&k| args.split_off(k)).collect();
        later.reverse();
        let mut call = first(args);
        let mut binds = Vec::with_capacity(later.len());
        for segment in later {
            let thunk = self.ctx.fresh_name("split");
            let head = Spanned::with_span(span, CompKind::Force(Val::Variable(thunk.clone())));
            binds.push((thunk, call));
            call = applied(span, head, segment);
        }
        binds
            .into_iter()
            .rev()
            .fold(call, |rest, (thunk, rhs)| {
                Spanned::with_span(span, CompKind::bind(Pattern::Name(thunk), rhs, rest))
            })
            .item
    }

    /// An `Exec`: its site from the checker, η-expanded when it is an
    /// under-applied boundary, split when its binding returned a thunk.
    fn exec(&mut self, comp: &Comp, e: &Exec) -> CompKind {
        let redirects = e.redirects.map(|v| self.val(v));
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
            let args = self.bound_args(&e.argv);
            let call = self.app(comp, force, args);
            return redirected(Spanned::with_span(comp.span, call), redirects);
        }
        let site = self.ctx.sites.get(&Node::comp(comp)).cloned();
        // An under-applied boundary is a boundary held as a value: a block of the
        // saturated call, wherever it stands.
        if let Some(missing) = self.ctx.boundary_missing.get(&Node::comp(comp)).copied() {
            let (params, args) = eta(self.ctx, missing);
            let mut argv = self.argv(&e.argv);
            argv.extend(args.into_iter().map(ValListElem::Single));
            let exec = CompKind::Exec(Exec {
                head: e.head.clone(),
                argv,
                redirects,
                site,
            });
            return lambda_comp(comp.span, params, exec).item;
        }
        let Some(splits) = self.ctx.splits.get(&Node::comp(comp)).cloned() else {
            return CompKind::Exec(Exec {
                head: e.head.clone(),
                argv: self.argv(&e.argv),
                redirects,
                site,
            });
        };
        let args = self.bound_args(&e.argv);
        let call = self.split(comp.span, &splits, args, |args| {
            Spanned::with_span(
                comp.span,
                CompKind::Exec(Exec {
                    head: e.head.clone(),
                    argv: args.into_iter().map(ValListElem::Single).collect(),
                    redirects: Redirects::default(),
                    site,
                }),
            )
        });
        redirected(Spanned::with_span(comp.span, call), redirects)
    }

    fn val(&mut self, val: &Val) -> Val {
        self.val_at(val, None)
    }

    /// [`Self::val`] for a value written at `span`, which the block a boundary
    /// reference or an adapted value becomes is given, so its errors point where
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
        match self.ctx.adapted_vals.get(&Node::val(val)) {
            Some(&(arity, how)) => adapted_block(rebuilt, arity, how, span, self.ctx),
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
            Val::Record(entries) => Val::Record(self.fields(entries)),
            Val::Map(entries) => Val::Map(self.fields(entries)),
            Val::Variant { label, payload } => Val::Variant {
                label: label.clone(),
                payload: Box::new(self.val(payload)),
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

    fn argv(&mut self, argv: &Argv) -> Argv {
        argv.iter().map(|e| self.list_elem(e)).collect()
    }

    fn args<'v>(&mut self, args: impl IntoIterator<Item = &'v Spanned<Val>>) -> Vec<Spanned<Val>> {
        args.into_iter().map(|arg| self.spanned_val(arg)).collect()
    }

    /// A binding's argv, as the arguments it is applied to: the checker
    /// refuses a spread into a binding.
    fn bound_args(&mut self, argv: &Argv) -> Vec<Spanned<Val>> {
        let args = argv
            .positional()
            .expect("the checker refuses a spread into a binding");
        self.args(args)
    }

    fn fields(&mut self, fields: &FieldsNode) -> Arc<FieldsNode> {
        FieldsNode::new(
            fields
                .shape()
                .iter()
                .map(|(name, value)| (name.clone(), self.spanned_val(value)))
                .collect(),
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::FileId;

    #[test]
    fn upcast_nodes_take_the_bodys_span_under_the_lambda_chain() {
        let span = Span::new(FileId::DUMMY, 3, 9);
        let mut ctx = InferCtx::new();
        let Val::Thunk(block) = adapted_block(
            Val::Variable("f".into()),
            2,
            Adaptation::Upcast,
            Some(span),
            &mut ctx,
        ) else {
            panic!("an adapted value is a block");
        };
        let mut body = block.shape();
        for _ in 0..2 {
            assert_eq!(body.span, Some(span));
            let CompKind::Lam { body: inner, .. } = &body.item else {
                panic!("the adaptation belongs under both lambdas");
            };
            body = inner;
        }
        let CompKind::Upcast(call) = &body.item else {
            panic!("the call is upcast");
        };
        let CompKind::App { head, args } = &call.item else {
            panic!("the value receives both arguments");
        };
        assert_eq!(args.len(), 2);
        assert!(matches!(head.item, CompKind::Force(_)));
        assert_eq!([body.span, call.span, head.span], [Some(span); 3]);
    }

    #[test]
    fn capture_nodes_take_the_bodys_span() {
        let span = Span::new(FileId::DUMMY, 3, 9);
        let mut ctx = InferCtx::new();

        let body = Spanned::new(span, CompKind::Return(Val::Unit));
        let CompKind::Bind { comp, rest, .. } = captured_string(body, &mut ctx) else {
            panic!("a capture is bound to its decode");
        };
        assert!(matches!(comp.item, CompKind::Capture(_)));
        assert!(matches!(rest.item, CompKind::Prim(Prim::Decode(_))));
        assert_eq!([comp.span, rest.span], [Some(span); 2]);

        let Val::Thunk(block) =
            adapted_block(Val::Unit, 1, Adaptation::Capture, Some(span), &mut ctx)
        else {
            panic!("a captured value is wrapped in a block");
        };
        let lam = block.shape();
        let CompKind::Lam { body, .. } = &lam.item else {
            panic!("the block takes its parameter");
        };
        let CompKind::Bind { comp, rest, .. } = &body.item else {
            panic!("the block's body is a capture and its decode");
        };
        let CompKind::Capture(call) = &comp.item else {
            panic!("the bound computation is the capture");
        };
        let spans = [lam.span, body.span, comp.span, rest.span, call.span];
        assert_eq!(spans, [Some(span); 5]);
    }
}
