//! Freeze solved sites and schemes, preserving shared groups.

use super::env::{InferCtx, Pending};
use super::generalize::{debug_assert_scheme_closed, settle_weak};
use crate::ir::{
    Argv, CaseArm, Comp, CompKind, DefineSchemes, Exec, FieldsNode, GroupNode, HandlerArmV, Phrase,
    Toplevel, Val, ValListElem,
};
use crate::source::Spanned;
use crate::ty::{Site, UnitId};
use std::collections::HashMap;
use std::sync::Arc;

struct Freeze<'a> {
    ctx: &'a InferCtx,
    unit: UnitId,
    groups: HashMap<*const GroupNode<Pending>, Arc<GroupNode>>,
}

impl Freeze<'_> {
    fn site(&self, site: &Pending) -> Arc<Site> {
        match site {
            Pending::Kept(site) => Arc::clone(site),
            Pending::Open(ty) | Pending::Imported(ty) => {
                Arc::new(Site::snapshot(&self.ctx.unifier, ty, self.unit))
            }
        }
    }

    fn rec_group(&mut self, group: &Arc<GroupNode<Pending>>) -> Arc<GroupNode> {
        let key = Arc::as_ptr(group);
        if let Some(rebuilt) = self.groups.get(&key) {
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
        self.groups.insert(key, Arc::clone(&rebuilt));
        rebuilt
    }

    fn comp(&mut self, comp: &Comp<Pending>) -> Comp {
        let item = match &comp.item {
            CompKind::Bind {
                comp: rhs,
                pattern,
                rest,
            } => CompKind::Bind {
                comp: Arc::new(self.comp(rhs)),
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
            CompKind::App { head, args } => CompKind::App {
                head: Arc::new(self.comp(head)),
                args: args.iter().map(|arg| self.spanned_val(arg)).collect(),
            },
            CompKind::Protect { body, site } => CompKind::Protect {
                body: Arc::new(self.comp(body)),
                site: self.site(site),
            },
            CompKind::Force(value) => CompKind::Force(self.val(value)),
            CompKind::Return(value) => CompKind::Return(self.val(value)),
            CompKind::Exec(e) => CompKind::Exec(Exec {
                head: e.head.clone(),
                argv: self.argv(&e.argv),
                redirects: e.redirects.map(|v| self.val(v)),
                site: e.site.as_ref().map(|site| self.site(site)),
            }),
            CompKind::Native { .. } => {
                unreachable!("a native's code is built by forcing it, never checked")
            }
            CompKind::Prim(prim) => CompKind::Prim(prim.map_operands(|value, _| self.val(value))),
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
        };
        Spanned::with_span(comp.span, item)
    }

    fn val(&mut self, val: &Val<Pending>) -> Val {
        match val {
            Val::Protected { value, site } => {
                let value = self.val(value);
                let frozen = self.site(site);
                if matches!(site, Pending::Imported(_)) && !frozen.has_delayed() {
                    value
                } else {
                    Val::Protected {
                        value: Box::new(value),
                        site: frozen,
                    }
                }
            }
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
            Val::Unit => Val::Unit,
            Val::String(v) => Val::String(v.clone()),
            Val::Int(v) => Val::Int(*v),
            Val::Float(v) => Val::Float(*v),
            Val::Bool(v) => Val::Bool(*v),
            Val::Variable(v) => Val::Variable(v.clone()),
        }
    }

    fn spanned_val(&mut self, value: &Spanned<Val<Pending>>) -> Spanned<Val> {
        Spanned::with_span(value.span, self.val(&value.item))
    }

    fn list_elem(&mut self, elem: &ValListElem<Pending>) -> ValListElem {
        match elem {
            ValListElem::Single(v) => ValListElem::Single(self.spanned_val(v)),
            ValListElem::Spread(v) => ValListElem::Spread(self.spanned_val(v)),
        }
    }

    fn argv(&mut self, argv: &Argv<Pending>) -> Argv {
        argv.iter().map(|e| self.list_elem(e)).collect()
    }

    fn fields(&mut self, fields: &FieldsNode<Pending>) -> Arc<FieldsNode> {
        FieldsNode::new(
            fields
                .shape()
                .iter()
                .map(|(name, value)| (name.clone(), self.spanned_val(value)))
                .collect(),
        )
    }
}

pub(super) fn freeze_toplevel(
    phrases: &[Spanned<Phrase<DefineSchemes, Pending>>],
    ctx: &InferCtx,
    unit: UnitId,
) -> Toplevel {
    let mut walk = Freeze {
        ctx,
        unit,
        groups: HashMap::new(),
    };
    let phrases = phrases
        .iter()
        .map(|phrase| {
            let item = match &phrase.item {
                Phrase::Define {
                    pattern,
                    comp,
                    schemes,
                } => {
                    let schemes = schemes
                        .iter()
                        .map(|(name, scheme)| {
                            let scheme = settle_weak(&ctx.unifier, Arc::clone(scheme));
                            debug_assert_scheme_closed(
                                &scheme,
                                "settled Define scheme must be closed",
                            );
                            (name.clone(), scheme)
                        })
                        .collect();
                    Phrase::Define {
                        pattern: Arc::clone(pattern),
                        comp: Arc::new(walk.comp(comp)),
                        schemes,
                    }
                }
                Phrase::Run(comp) => Phrase::Run(Arc::new(walk.comp(comp))),
            };
            Spanned::with_span(phrase.span, item)
        })
        .collect();
    let admits = ctx
        .residual_uses
        .iter()
        .map(|(name, ty)| {
            (
                name.clone(),
                Arc::new(Site::snapshot(&ctx.unifier, ty, unit)),
            )
        })
        .collect();
    Toplevel { phrases, admits }
}
