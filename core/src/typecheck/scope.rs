//! Typing rules for the `within`, `grant`, `try`, `guard` and `audit` scope
//! nodes.  The sixth scope node, `CompKind::Redirect`, is typed inline in
//! `infer.rs`.
//!
//! A form's options are syntax: the parser owns the bracket's shape and this
//! module owns its names.  Each written option is looked up in the table the
//! form declares in [`super::contract`] and held to what the table says of
//! it, key by written key; no record type is built for the bracket, so what
//! is not written is an absent premise.
//!
//! The runtime's own unknown-key refusals — `decode_capability_map`'s — are
//! not dead code: `use` *asserts* a module's row rather than checking it, so
//! a contract file can still hand a door a key no rule here ever saw.
//! `WithinScope::parse` has no such caller, and treats an unknown key as an
//! invariant.
//!
//! A scope form passes its body's producer through, grade included: what
//! `within [dir: d] { hostname }` is, is a command.

use super::builtins::{audit_record, try_error_record};
use super::contract::{Form, Holds, Table, catch_all_shape, declared, field_reason};
use super::error::{Reason, TypeErrorKind};
use super::grade::JoinArm;
use super::infer::Inferencer;
use super::scheme::Scheme;
use super::ty::{CompTy, Ty};
use crate::ir::{HandlerArmV, OptionsV, Val};
use crate::source::WithSpan;

impl Inferencer<'_> {
    /// Hold each written option to what `table` says of its label, each
    /// caret on the option's own value.  A catch-all arm written out is
    /// checked in context, its name and argv bound before its body, as a
    /// named arm is; a bound one is unified against the shape the table
    /// mints.
    fn check_options(&mut self, opts: &OptionsV, table: &'static Table) {
        for (label, value) in opts {
            self.with_span(value.span, |this| {
                this.check_option(label, &value.item, table);
            });
        }
        for key in table.keys {
            if matches!(key.holds, Holds::Required(_))
                && !opts.iter().any(|(l, _)| l.as_ref() == key.label)
            {
                self.ctx.diagnose(TypeErrorKind::RowMissingField {
                    label: key.label.to_string(),
                });
            }
        }
    }

    fn check_option(&mut self, label: &str, value: &Val, table: &'static Table) {
        let key = table.keys.iter().find(|k| k.label == label);
        let catch_all = key.is_some_and(|k| matches!(k.holds, Holds::Shaped(_)));
        let found = if catch_all {
            let cty = match value {
                Val::Thunk(comp) => self.infer_catch_all(comp.shape()),
                // A value in hand is pinned to the catch-all's parameters first, so
                // its producer is what stands in.
                value => {
                    let ty = self.infer_val(value);
                    let shape = catch_all_shape(self.ctx.unifier.fresh_comp_ty());
                    self.ctx
                        .unify_ty(&ty, &Ty::Thunk(Box::new(shape.clone())), Reason::HandlerArm);
                    shape
                }
            };
            let cty = match self.catch_all_stands_in(&cty) {
                Ok(cty) => cty,
                Err((cty, error)) => {
                    self.ctx.raise(*error);
                    cty
                }
            };
            Ty::Thunk(Box::new(cty))
        } else {
            self.infer_val(value)
        };
        let Some(key) = key else {
            return self.ctx.diagnose(TypeErrorKind::UnknownKey {
                form: table.form,
                key: label.to_string(),
                offered: table.offered(),
            });
        };
        let declared = match &key.holds {
            Holds::At(ty) | Holds::Required(ty) => ty.clone(),
            Holds::Shaped(shape) => shape(&mut self.ctx.unifier),
            Holds::Decoded => return,
            Holds::Refused(advice) => {
                return self.ctx.diagnose(TypeErrorKind::RefusedKey {
                    form: table.form,
                    key: key.label,
                    advice,
                });
            }
        };
        self.ctx
            .unify_ty(&declared, &found, field_reason(table, key));
    }

    /// Collect the schemes `handlers:` binds in the body.  A catch-all
    /// `handler:` matches every name, so no one name can be bound by it.
    fn handler_bindings(&mut self, arms: Option<&[HandlerArmV]>) -> Vec<(String, Scheme)> {
        arms.unwrap_or_default()
            .iter()
            .map(|arm| {
                let scheme = self.with_span(arm.value.span, |this| {
                    this.handler_arm_scheme(&arm.name, &arm.value.item)
                });
                (arm.name.clone(), scheme)
            })
            .collect()
    }

    pub(super) fn infer_within(
        &mut self,
        opts: &OptionsV,
        handlers: Option<&[HandlerArmV]>,
        body: &Val,
    ) -> CompTy {
        self.check_options(opts, declared(Form::Within));
        let bindings = self.handler_bindings(handlers);

        self.env.push();
        for (name, scheme) in bindings {
            self.env.bind_handler(name, scheme, false);
        }
        let body_cty = self.scope_body(body);
        self.env.pop();
        body_cty
    }

    pub(super) fn infer_grant(&mut self, caps: &OptionsV, body: &Val) -> CompTy {
        self.check_options(caps, declared(Form::Grant));
        self.scope_body(body)
    }

    /// `try` produces what its body or its handler produces, so the two
    /// join: `T : U B`, `H : U (Error → B)`.
    pub(super) fn infer_try(&mut self, body: &Val, handler: &Val) -> CompTy {
        let arms = [
            JoinArm::in_hand(body, vec![], Reason::ScopeBody),
            JoinArm::in_hand(handler, vec![try_error_record()], Reason::TryHandler),
        ];
        self.join_arms(&arms, &Reason::TryArms)
    }

    /// `guard` produces what its body does; `cleanup` runs for its effects
    /// and errors only.
    pub(super) fn infer_guard(&mut self, body: &Val, cleanup: &Val) -> CompTy {
        let body_cty = self.scope_body(body);
        let _ = self.scope_body(cleanup);
        body_cty
    }

    /// `audit` absorbs its body, whatever it produces: the `` `ok `` payload
    /// is the body's raw value, which the runtime stores undecoded.
    pub(super) fn infer_audit(&mut self, body: &Val) -> CompTy {
        let body_cty = self.scope_body(body);
        let alpha = self.extract_return(&body_cty);
        CompTy::pure(audit_record(alpha))
    }

    /// Constrain `body` to a thunk of a computation ready to run, and return
    /// that computation — a `Return` at the body's own grade.
    fn scope_body(&mut self, body: &Val) -> CompTy {
        let body_cty = self.ctx.unifier.fresh_comp_ty();
        let body_ty = self.infer_val(body);
        self.ctx.unify_ty(
            &body_ty,
            &Ty::Thunk(Box::new(body_cty.clone())),
            Reason::ScopeBody,
        );
        let _ = self.extract_return(&body_cty);
        body_cty
    }
}
