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
//! No rule here builds a `CompTy` directly: each states the value a scope
//! produces, for its caller to compose into `CompTy::pure`.

use super::builtins::{audit_record, try_error_record};
use super::contract::{Form, Holds, Table, declared, field_reason};
use super::error::{Reason, TypeErrorKind};
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
        let found = match value {
            Val::Thunk(comp) if catch_all => {
                Ty::Thunk(Box::new(self.infer_catch_all(comp.shape())))
            }
            _ => self.infer_val(value),
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
    ) -> Ty {
        self.check_options(opts, declared(Form::Within));
        let bindings = self.handler_bindings(handlers);

        self.env.push();
        for (name, scheme) in bindings {
            self.env.bind_handler(name, scheme, false);
        }
        let body_cty = self.infer_scope_body_passthrough(body);
        self.env.pop();

        self.extract_return(&body_cty)
    }

    pub(super) fn infer_grant(&mut self, caps: &OptionsV, body: &Val) -> Ty {
        self.check_options(caps, declared(Form::Grant));
        let body_cty = self.infer_scope_body_passthrough(body);
        self.extract_return(&body_cty)
    }

    /// `try` yields the body's value or the handler's, so the two types unify:
    /// `T : U (F A)`, `H : U (Error → F A)`.
    pub(super) fn infer_try(&mut self, body: &Val, handler: &Val) -> Ty {
        let body_cty = self.infer_scope_body_passthrough(body);
        let body_ty = self.extract_return(&body_cty);

        let handler_result_cty = self.ctx.unifier.fresh_comp_ty();
        let handler_inner = CompTy::Fun(
            Box::new(try_error_record()),
            Box::new(handler_result_cty.clone()),
        );
        let handler_ty = self.infer_val(handler);
        self.ctx.unify_ty(
            &handler_ty,
            &Ty::Thunk(Box::new(handler_inner)),
            Reason::TryHandler,
        );
        let handler_value = self.extract_return(&handler_result_cty);

        let why = Reason::TryArms {
            writer: self.arm_writer(body).or_else(|| self.arm_writer(handler)),
        };
        self.ctx.unify_ty(&body_ty, &handler_value, why);
        body_ty
    }

    /// `guard`'s value passes through from its body; `cleanup` runs for its
    /// effects and errors only.
    pub(super) fn infer_guard(&mut self, body: &Val, cleanup: &Val) -> Ty {
        let body_cty = self.infer_scope_body_passthrough(body);
        let value = self.extract_return(&body_cty);

        let _ = self.infer_scope_body_passthrough(cleanup);

        value
    }

    pub(super) fn infer_audit(&mut self, body: &Val) -> Ty {
        let body_cty = self.infer_scope_body_passthrough(body);
        // The `` `ok `` payload is the body's raw result — the runtime stores
        // it undecoded.
        let alpha = self.extract_return(&body_cty);

        audit_record(alpha)
    }

    /// Constrain `body` to `Thunk(c)` for a bare fresh comp var `c`, and
    /// return `c`; callers read it back with `extract_return` once resolved.
    fn infer_scope_body_passthrough(&mut self, body: &Val) -> CompTy {
        let body_cty = self.ctx.unifier.fresh_comp_ty();
        let body_ty = self.infer_val(body);
        self.ctx.unify_ty(
            &body_ty,
            &Ty::Thunk(Box::new(body_cty.clone())),
            Reason::ScopeBody,
        );
        body_cty
    }
}
