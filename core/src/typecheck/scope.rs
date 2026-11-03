//! Typing rules for the `within`, `grant`, `try`, `guard`, `audit`, `spawn`
//! and `gather` scope nodes.  The eighth scope node, `CompKind::Redirect`, is
//! typed inline in `infer.rs`.
//!
//! A form's options are syntax: the parser owns the bracket's shape and this
//! module owns its names.  Each written option is looked up in the table the
//! form declares in [`super::contract`] and held to what the table says of
//! it, key by written key; no record type is built for the bracket, so what
//! is not written is an absent premise.  A value held to a nested table is
//! inferred as any other and queued, to be ascribed once inference has
//! finished (`contract::drain_nested`).
//!
//! The runtime's own unknown-key refusals — `decode_capability_map`'s — are
//! not dead code: `use` *asserts* a module's row rather than checking it, so
//! a contract file can still hand a door a key no rule here ever saw.
//! `WithinScope::parse` has no such caller, and treats an unknown key as an
//! invariant.
//!
//! A scope form passes its body's producer through, grade included: what
//! `within [dir: d] { hostname }` is, is a command.

use super::builtins::audit_record;
use super::contract::{Form, Held, Holds, Key, NestedCheck, Table, declared, unify_held};
use super::env::{HandlerOrigin, Pending};
use super::error::{Reason, TypeErrorKind};
use super::grade::JoinArm;
use super::infer::{Arm, Inferencer};
use crate::fact::ErrorRecord;
use crate::ir::{CompKind, FieldsNode, HandlerArmV, Val};
use crate::source::{Span, Spanned, WithSpan};
use crate::ty::Typed as _;
use crate::ty::{CompTy, Producer, Scheme, Ty};
use std::sync::Arc;

impl Inferencer<'_> {
    /// Hold each written option to what `table` says of its label, each
    /// caret on the option's own value.  A catch-all is held to the arm rule
    /// at its slot ([`Inferencer::arm`]).
    fn check_options(
        &mut self,
        opts: &FieldsNode,
        table: &'static Table,
    ) -> Arc<FieldsNode<Pending>> {
        // Check in span order; assemble in the fields' original key order.
        let mut written: Vec<_> = opts.shape().iter().enumerate().collect();
        written.sort_by_key(|(_, (_, value))| value.span.map(|span| span.start));
        let mut nodes: Vec<_> = (0..written.len()).map(|_| None).collect();
        for (i, (label, value)) in written {
            let node = self.with_span(value.span, |this| {
                this.check_option(label, &value.item, value.span, table)
            });
            nodes[i] = Some((label.clone(), Spanned::with_span(value.span, node)));
        }
        for key in table.keys {
            if matches!(key.holds, Holds::Required(_))
                && !opts.shape().iter().any(|(l, _)| l.as_ref() == key.label)
            {
                self.ctx.diagnose(TypeErrorKind::RowMissingField {
                    label: key.label.to_string(),
                });
            }
        }
        FieldsNode::new(
            nodes
                .into_iter()
                .map(|node| node.expect("option inferred"))
                .collect(),
        )
    }

    fn check_option(
        &mut self,
        label: &str,
        value: &Val,
        at: Option<Span>,
        table: &'static Table,
    ) -> Val<Pending> {
        let key = table.keys.iter().find(|k| k.label == label);
        let (found, node) = match key {
            Some(Key {
                holds: Holds::CatchAll,
                ..
            }) => self.infer_catch_all_option(value, at),
            _ => self.infer_val(value, at),
        };
        let Some(key) = key else {
            self.ctx.diagnose(TypeErrorKind::UnknownKey {
                form: table.form,
                key: label.to_string(),
                offered: table.offered(),
            });
            return node;
        };
        match key.holds.held(&mut self.ctx.unifier) {
            Held::Unify(declared) => {
                self.ctx
                    .unify_ty_with(&declared, &found, unify_held(table, key));
            }
            Held::Nested(sub) => self.ctx.nested.push(NestedCheck {
                key: key.label,
                table: sub,
                ty: found,
                pos: self.ctx.pos,
            }),
            Held::Refuse(advice) => self.ctx.diagnose(TypeErrorKind::RefusedKey {
                form: table.form,
                key: key.label,
                advice,
            }),
            Held::Nothing => {}
        }
        node
    }

    /// The catch-all `handler:`: the arm rule at the catch-all's slot.
    fn infer_catch_all_option(&mut self, value: &Val, at: Option<Span>) -> (Ty, Val<Pending>) {
        let (cty, node) = self.checked_arm(value, Arm::CatchAll, at);
        (Ty::Thunk(Box::new(cty)), node)
    }

    /// Named arms bind schemes; a catch-all matches every name, binding none.
    #[allow(
        clippy::type_complexity,
        reason = "schemes beside rebuilt handler arms"
    )]
    fn handler_bindings(
        &mut self,
        arms: Option<&[HandlerArmV]>,
    ) -> (Vec<(String, Scheme)>, Option<Vec<HandlerArmV<Pending>>>) {
        let mut bindings = Vec::new();
        let nodes = arms.map(|arms| {
            arms.iter()
                .map(|arm| {
                    let (scheme, value) = self.with_span(arm.value.span, |this| {
                        this.install_arm(&arm.value.item, &arm.name, arm.value.span)
                    });
                    bindings.push((arm.name.clone(), scheme));
                    HandlerArmV {
                        name: arm.name.clone(),
                        value: Spanned::with_span(arm.value.span, value),
                    }
                })
                .collect()
        });
        (bindings, nodes)
    }

    pub(super) fn infer_within(
        &mut self,
        opts: &FieldsNode,
        handlers: Option<&[HandlerArmV]>,
        body: &Val,
    ) -> (CompTy, CompKind<Pending>) {
        let opts = self.check_options(opts, declared(Form::Within));
        let (bindings, handlers) = self.handler_bindings(handlers);
        let (cty, body) = self.with_scope(|this| {
            for (name, scheme) in bindings {
                this.env.bind_handler(name, scheme, HandlerOrigin::Within);
            }
            this.scope_body(body)
        });
        (
            cty,
            CompKind::Within {
                opts,
                handlers,
                body,
            },
        )
    }

    pub(super) fn infer_grant(
        &mut self,
        caps: &FieldsNode,
        body: &Val,
    ) -> (CompTy, CompKind<Pending>) {
        let caps = self.check_options(caps, declared(Form::Grant));
        let (cty, body) = self.scope_body(body);
        (cty, CompKind::Grant { caps, body })
    }

    /// Join the outcomes: `T : U B`, `H : U (Error → B)`.
    pub(super) fn infer_try(&mut self, body: &Val, handler: &Val) -> (CompTy, CompKind<Pending>) {
        let arms = [
            JoinArm::in_hand(body, vec![], Reason::ScopeBody),
            JoinArm::in_hand(handler, vec![ErrorRecord::ty()], Reason::TryHandler),
        ];
        let (cty, nodes) = self.join_arms(&arms, &Reason::TryArms);
        let mut nodes = nodes.into_iter();
        (
            cty,
            CompKind::Try {
                body: nodes.next().expect("try body"),
                handler: nodes.next().expect("try handler"),
            },
        )
    }

    /// Preserve the body's producer; cleanup runs only for effects and errors.
    pub(super) fn infer_guard(&mut self, body: &Val, cleanup: &Val) -> (CompTy, CompKind<Pending>) {
        let (cty, body) = self.scope_body(body);
        let (_, cleanup) = self.scope_body(cleanup);
        (cty, CompKind::Guard { body, cleanup })
    }

    /// Store the body's raw, undecoded value as the `ok` payload, at a fresh grade.
    pub(super) fn infer_audit(&mut self, body: &Val) -> (CompTy, CompKind<Pending>) {
        let (cty, body) = self.scope_body(body);
        let alpha = self.extract_return(&cty);
        (self.produces(audit_record(alpha)), CompKind::Audit { body })
    }

    /// `spawn [T] B : F^ν (Handle^ε α)` for `B : U(F^ε α)`, `T : String`; ν fresh.
    pub(super) fn infer_spawn(
        &mut self,
        title: Option<&Spanned<Val>>,
        body: &Val,
    ) -> (CompTy, CompKind<Pending>) {
        let title = title.map(|title| {
            self.with_span(title.span, |this| {
                let (ty, node) = this.infer_val(&title.item, title.span);
                this.ctx.unify_ty(&ty, &Ty::String, Reason::SpawnTitle);
                Spanned::with_span(title.span, node)
            })
        });
        let (cty, body) = self.scope_body(body);
        let Producer { grade, ty } = self.force_return_shape(&cty, Reason::ReturnShape);
        (
            self.produces(Ty::Handle(grade, Box::new(ty))),
            CompKind::Spawn { title, body },
        )
    }

    /// Preserve the body's producer; joining the brood is a runtime effect.
    pub(super) fn infer_gather(&mut self, body: &Val) -> (CompTy, CompKind<Pending>) {
        let (cty, body) = self.scope_body(body);
        (cty, CompKind::Gather { body })
    }

    /// Require a ready-to-run thunk, retaining its own producer grade.
    fn scope_body(&mut self, body: &Val) -> (CompTy, Val<Pending>) {
        let body_cty = self.ctx.unifier.fresh_comp_ty();
        let (body_ty, body) = self.infer_val(body, None);
        self.ctx.unify_ty(
            &body_ty,
            &Ty::Thunk(Box::new(body_cty.clone())),
            Reason::ScopeBody,
        );
        let _ = self.extract_return(&body_cty);
        (body_cty, body)
    }
}
