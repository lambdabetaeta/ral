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
use super::env::HandlerOrigin;
use super::error::{Reason, TypeErrorKind};
use super::grade::JoinArm;
use super::infer::{Arm, Inferencer};
use crate::fact::ErrorRecord;
use crate::ir::{HandlerArmV, OptionsV, Val};
use crate::source::{Spanned, WithSpan};
use crate::ty::Typed as _;
use crate::ty::{CompTy, Producer, Scheme, Ty};

impl Inferencer<'_> {
    /// Hold each written option to what `table` says of its label, each
    /// caret on the option's own value.  A catch-all is held to the arm rule
    /// at its slot ([`Inferencer::arm`]).
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
        let found = match key {
            Some(Key {
                holds: Holds::CatchAll,
                ..
            }) => self.infer_catch_all_option(value),
            _ => self.infer_val(value),
        };
        let Some(key) = key else {
            return self.ctx.diagnose(TypeErrorKind::UnknownKey {
                form: table.form,
                key: label.to_string(),
                offered: table.offered(),
            });
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
    }

    /// The catch-all `handler:`: the arm rule at the catch-all's slot.
    fn infer_catch_all_option(&mut self, value: &Val) -> Ty {
        Ty::Thunk(Box::new(self.checked_arm(value, Arm::CatchAll)))
    }

    /// Collect the schemes `handlers:` binds in the body.  A catch-all
    /// `handler:` matches every name, so no one name can be bound by it.
    fn handler_bindings(&mut self, arms: Option<&[HandlerArmV]>) -> Vec<(String, Scheme)> {
        arms.unwrap_or_default()
            .iter()
            .map(|arm| {
                let scheme = self.with_span(arm.value.span, |this| {
                    this.install_arm(&arm.value.item, &arm.name)
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

        self.with_scope(|this| {
            for (name, scheme) in bindings {
                this.env.bind_handler(name, scheme, HandlerOrigin::Within);
            }
            this.scope_body(body)
        })
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
            JoinArm::in_hand(handler, vec![ErrorRecord::ty()], Reason::TryHandler),
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

    /// `spawn [T] B : F (Handle^ε α)` for `B : U(F^ε α)`; `T : String`.
    pub(super) fn infer_spawn(&mut self, title: Option<&Spanned<Val>>, body: &Val) -> CompTy {
        if let Some(title) = title {
            self.with_span(title.span, |this| {
                let ty = this.infer_val(&title.item);
                this.ctx.unify_ty(&ty, &Ty::String, Reason::SpawnTitle);
            });
        }
        let body_cty = self.scope_body(body);
        let Producer { grade, ty } = self.force_return_shape(&body_cty, Reason::ReturnShape);
        CompTy::pure(Ty::Handle(grade, Box::new(ty)))
    }

    /// `gather BODY : F^ε α` for `BODY : U(F^ε α)`: the body's type passes
    /// through; joining the brood is a run-time effect.
    pub(super) fn infer_gather(&mut self, body: &Val) -> CompTy {
        self.scope_body(body)
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
