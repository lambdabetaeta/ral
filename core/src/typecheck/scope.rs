//! Typing rules for the `within`, `grant`, `try`, `guard` and `audit` scope
//! nodes, plus the field schemas for the `within`/`grant` option maps.  The
//! sixth scope node, `ScopeOp::Redirect`, is typed inline in `infer.rs`.
//!
//! Scope bodies stay mode-polymorphic (`F[μ₀,μ₁] α`) rather than pinned to
//! `F[none,none]`: mode unification is equality-strict, so a pinned `none`
//! would clash with any body that writes bytes (`try { echo x }`).  Unknown
//! option keys are rejected at runtime — by `WithinScope::parse` and
//! `decode_capability_map` — not by the schemas here.

use super::builtins::{FieldSchema, audit_record, try_error_record};
use super::error::Reason;
use super::infer::Inferencer;
use super::scheme::Scheme;
use super::ty::{CompTy, PipeSpec, Ty};
use super::unify::Unifier;
use crate::ir::{Val, ValMapEntry};

impl Inferencer<'_> {
    /// Collect handler schemes from literal `handlers: [name: { … }]` entries;
    /// everything else is inferred for its errors and binds nothing — a
    /// catch-all `handler:` matches every name, so no one name can be bound.
    /// Each thunk is inferred exactly once, so an error inside is reported once.
    fn infer_within_opts(&mut self, opts: &Val) -> Vec<(String, Scheme)> {
        let Val::Map(outer_entries) = opts else {
            let _ = self.infer_val(opts);
            return Vec::new();
        };

        let mut bindings = Vec::new();

        for outer_entry in outer_entries {
            match outer_entry {
                ValMapEntry::Entry(Val::String(key), Val::Map(inner_entries))
                    if key == "handlers" =>
                {
                    for inner_entry in inner_entries {
                        match inner_entry {
                            ValMapEntry::Entry(Val::String(name), Val::Thunk(comp)) => {
                                bindings.push((name.clone(), self.handler_comp_scheme(name, comp)));
                            }
                            ValMapEntry::Entry(key_val, val_val) => {
                                let _ = self.infer_val(key_val);
                                let _ = self.infer_val(val_val);
                            }
                            ValMapEntry::Spread(val) => {
                                let _ = self.infer_val(val);
                            }
                        }
                    }
                }

                ValMapEntry::Entry(Val::String(key), Val::Thunk(comp)) if key == "handler" => {
                    let _ = self.with_scope(|this| this.infer_comp(comp));
                }

                entry => {
                    self.check_map_entry_fields(
                        std::slice::from_ref(entry),
                        "within",
                        within_field_ty,
                    );
                }
            }
        }

        bindings
    }

    pub(super) fn infer_within(&mut self, opts: &Val, body: &Val) -> CompTy {
        let bindings = self.infer_within_opts(opts);

        self.env.push();
        for (name, scheme) in bindings {
            self.env.bind_handler(name, scheme, false);
        }
        let result = self.infer_scope_body_passthrough(body);
        self.env.pop();
        result
    }

    pub(super) fn infer_grant(&mut self, caps: &Val, body: &Val) -> CompTy {
        self.infer_scope_opts(caps, "grant", grant_field_ty);
        self.infer_scope_body_passthrough(body)
    }

    pub(super) fn infer_try(&mut self, body: &Val, handler: &Val) -> CompTy {
        let body_cty = self.infer_scope_body_passthrough(body);
        let (body_raw, body_in, body_out) = self.extract_return(&body_cty);
        let body_final_out = self.final_output_of_thunk_value(body, &body_cty);
        let body_value = self.observed_value_ty(body_raw, body_final_out);

        // `try` yields the body's value or the handler's, so the two observed
        // types unify; the handler's own pipeline modes stay independent.
        let handler_value_raw = self.ctx.unifier.fresh_ty();
        let handler_result =
            CompTy::Return(self.ctx.unifier.fresh_spec(), Box::new(handler_value_raw));
        let handler_inner = CompTy::Fun(
            Box::new(try_error_record()),
            Box::new(handler_result.clone()),
        );
        let handler_ty = self.infer_val(handler);
        self.ctx.unify_ty(
            &handler_ty,
            &Ty::Thunk(Box::new(handler_inner)),
            Reason::TryHandler,
        );

        let (handler_raw, handler_in, handler_out) = self.extract_return(&handler_result);
        let handler_final_out = self.final_output_of_thunk_value(handler, &handler_result);
        let handler_value = self.observed_value_ty(handler_raw, handler_final_out);
        self.ctx
            .unify_ty(&body_value, &handler_value, Reason::TryArms);

        CompTy::Return(
            PipeSpec {
                input: self.union_mode(body_in, handler_in),
                output: self.join_byte_output(body_out, handler_out),
            },
            Box::new(body_value),
        )
    }

    pub(super) fn infer_guard(&mut self, body: &Val, cleanup: &Val) -> CompTy {
        let alpha = self.infer_thunk_body(body);
        let _beta = self.infer_thunk_body(cleanup);
        CompTy::pure(alpha)
    }

    pub(super) fn infer_audit(&mut self, body: &Val) -> CompTy {
        let alpha = self.infer_thunk_body(body);
        let beta = self.ctx.unifier.fresh_ty();
        CompTy::pure(audit_record(alpha, beta))
    }

    fn infer_scope_opts(&mut self, opts: &Val, form: &'static str, schema: FieldSchema) {
        match opts {
            Val::Map(entries) => self.check_map_entry_fields(entries, form, schema),
            _ => {
                let _ = self.infer_val(opts);
            }
        }
    }

    fn infer_scope_body_passthrough(&mut self, body: &Val) -> CompTy {
        let alpha = self.ctx.unifier.fresh_ty();
        let body_cty = CompTy::Return(self.ctx.unifier.fresh_spec(), Box::new(alpha));

        let body_ty = self.infer_val(body);
        self.ctx.unify_ty(
            &body_ty,
            &Ty::Thunk(Box::new(body_cty.clone())),
            Reason::ScopeBody,
        );

        body_cty
    }

    /// Constrain `val` to `Thunk(F[μ₀,μ₁] α)` for a fresh `α` and fresh
    /// pipeline modes, and return `α`.
    fn infer_thunk_body(&mut self, val: &Val) -> Ty {
        match self.infer_scope_body_passthrough(val) {
            CompTy::Return(_, alpha) => *alpha,
            _ => unreachable!("infer_scope_body_passthrough always returns CompTy::Return"),
        }
    }
}

/// Schema for the `within [env:, dir:]` options map; `handlers:`/`handler:`
/// hold thunks and dispatch at runtime.
fn within_field_ty(key: &str, u: &mut Unifier) -> Option<Ty> {
    match key {
        "env" => Some(Ty::Map(Box::new(u.fresh_ty()))),
        "dir" => Some(Ty::String),
        _ => None,
    }
}

/// Schema for the `grant [exec:, fs:, net:, detach:, audit:, editor:, shell:]`
/// map.  `exec` and `fs` are left to `decode_capability_map`: an `exec` policy
/// value is either an `'allow'`/`'deny'` string or a subcommand list, and the
/// two mix freely within one map, so no homogeneous element type fits.  Their
/// values are still inferred, so an error inside a policy expression surfaces.
fn grant_field_ty(key: &str, _u: &mut Unifier) -> Option<Ty> {
    let bool_map = || Ty::Map(Box::new(Ty::Bool));
    match key {
        "net" | "detach" | "audit" => Some(Ty::Bool),
        "editor" | "shell" => Some(bool_map()),
        _ => None,
    }
}
