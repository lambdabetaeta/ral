//! Typing rules for the `within`, `grant`, `try`, `guard` and `audit` scope
//! nodes, plus the field schemas for the `within`/`grant` option records.  The
//! sixth scope node, `CompKind::Redirect`, is typed inline in `infer.rs`.
//!
//! Unknown option keys are rejected at runtime — by `WithinScope::parse` and
//! `decode_capability_map` — not by the schemas here.
//!
//! No rule here builds a `CompTy` directly: each states the value a scope
//! produces and the route that carries it, and returns a [`ScopeSig`] for
//! its caller to compose into `CompTy::Return(sig.route, Box::new(sig.value))`.

use super::builtins::{FieldSchema, audit_record, try_error_record};
use super::error::{CompDiff, Reason, TypeErrorKind};
use super::infer::Inferencer;
use super::route::PayloadRoute;
use super::scheme::Scheme;
use super::ty::{CompTy, Ty};
use super::unify::Unifier;
use crate::ir::{Val, ValRecordEntry};
use crate::source::{Spanned, WithSpan};

/// What a scope rule knows before its computation type exists: the value the
/// scope produces and the route that carries it.
pub(super) struct ScopeSig {
    pub(super) value: Ty,
    pub(super) route: PayloadRoute,
}

impl Inferencer<'_> {
    /// Collect handler schemes from literal `handlers: [name: { … }]` entries;
    /// everything else is inferred for its errors and binds nothing — a
    /// catch-all `handler:` matches every name, so no one name can be bound.
    /// Each thunk is inferred exactly once, so an error inside is reported once.
    fn infer_within_opts(&mut self, opts: &Val) -> Vec<(String, Scheme)> {
        let Val::Record(outer_entries) = opts else {
            let _ = self.infer_val(opts);
            return Vec::new();
        };

        let mut bindings = Vec::new();

        for outer_entry in outer_entries {
            match outer_entry {
                ValRecordEntry::Field(
                    key,
                    Spanned {
                        item: Val::Record(inner_entries),
                        ..
                    },
                ) if key == "handlers" => {
                    for inner_entry in inner_entries {
                        match inner_entry {
                            ValRecordEntry::Field(
                                name,
                                value @ Spanned {
                                    item: Val::Thunk(comp),
                                    ..
                                },
                            ) => {
                                let scheme = self.with_span(value.span, |this| {
                                    this.handler_comp_scheme(name, comp)
                                });
                                bindings.push((name.clone(), scheme));
                            }
                            ValRecordEntry::Field(_, val_val) => {
                                let _ = self.infer_val(&val_val.item);
                            }
                            ValRecordEntry::Spread(val) => {
                                let _ = self.infer_val(&val.item);
                            }
                        }
                    }
                }

                ValRecordEntry::Field(
                    key,
                    value @ Spanned {
                        item: Val::Thunk(comp),
                        ..
                    },
                ) if key == "handler" => {
                    self.with_span(value.span, |this| {
                        let cty = this.infer_catch_all(comp);
                        let arm_return = this.alias_arm_body(&cty);
                        let (value, route) = this.extract_return(&arm_return);
                        if let Err(actual) = this.check_bytes_route(route, &value) {
                            let kind = TypeErrorKind::CompTyMismatch {
                                expected: CompTy::bytes(),
                                actual: CompTy::Return(
                                    PayloadRoute::Bytes,
                                    Box::new(actual.clone()),
                                ),
                                diffs: vec![CompDiff::ReturnType {
                                    expected: Ty::Unit,
                                    actual,
                                }],
                            };
                            this.ctx.report(kind, Reason::CatchAllRoutePin);
                        }
                    });
                }

                entry => {
                    self.check_record_entry_fields(
                        std::slice::from_ref(entry),
                        "within",
                        within_field_ty,
                    );
                }
            }
        }

        bindings
    }

    pub(super) fn infer_within(&mut self, opts: &Val, body: &Val) -> ScopeSig {
        let bindings = self.infer_within_opts(opts);

        self.env.push();
        for (name, scheme) in bindings {
            self.env.bind_handler(name, scheme, false);
        }
        let body_cty = self.infer_scope_body_passthrough(body);
        self.env.pop();

        let (value, route) = self.extract_return(&body_cty);
        self.ctx
            .val_results
            .insert(std::ptr::from_ref::<Val>(body) as usize, route);
        ScopeSig { value, route }
    }

    pub(super) fn infer_grant(&mut self, caps: &Val, body: &Val) -> ScopeSig {
        self.infer_scope_opts(caps, "grant", grant_field_ty);
        let body_cty = self.infer_scope_body_passthrough(body);

        let (value, route) = self.extract_return(&body_cty);
        self.ctx
            .val_results
            .insert(std::ptr::from_ref::<Val>(body) as usize, route);
        ScopeSig { value, route }
    }

    /// `try` joins body and handler via [`super::env::InferCtx::join_arm_results`],
    /// the same rule [`Inferencer::merge_branches`] uses for `if`/`?` arms.
    pub(super) fn infer_try(&mut self, body: &Val, handler: &Val) -> ScopeSig {
        let body_cty = self.infer_scope_body_passthrough(body);
        let (body_raw, body_route) = self.extract_return(&body_cty);
        self.ctx
            .val_results
            .insert(std::ptr::from_ref::<Val>(body) as usize, body_route);

        // `try` yields the body's value or the handler's, so the two joined
        // types unify; the handler's own route stays independent. A bare
        // fresh comp var, not a pre-built `Return`, is the expected shape: it
        // binds to the handler's actual type wholesale, route included,
        // rather than comparing a hardcoded placeholder against it.
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

        let (handler_raw, handler_route) = self.extract_return(&handler_result_cty);
        self.ctx
            .val_results
            .insert(std::ptr::from_ref::<Val>(handler) as usize, handler_route);

        let (route, value) = self.ctx.join_arm_results(
            vec![(body_route, body_raw), (handler_route, handler_raw)],
            Reason::TryArms,
        );

        ScopeSig { value, route }
    }

    /// `guard`'s value and route pass through from its body; `cleanup` runs
    /// for its effects and errors only — having no consumer for a payload,
    /// it escapes whatever it writes, exactly as a discarded statement does.
    pub(super) fn infer_guard(&mut self, body: &Val, cleanup: &Val) -> ScopeSig {
        let body_cty = self.infer_scope_body_passthrough(body);
        let (value, route) = self.extract_return(&body_cty);
        self.ctx
            .val_results
            .insert(std::ptr::from_ref::<Val>(body) as usize, route);

        let _ = self.infer_scope_body_passthrough(cleanup);

        ScopeSig { value, route }
    }

    pub(super) fn infer_audit(&mut self, body: &Val) -> ScopeSig {
        let body_cty = self.infer_scope_body_passthrough(body);
        // The `` `ok `` payload is the body's raw result — the runtime stores
        // it undecoded.
        let (alpha, _) = self.extract_return(&body_cty);

        ScopeSig {
            value: audit_record(alpha),
            route: PayloadRoute::Value,
        }
    }

    fn infer_scope_opts(&mut self, opts: &Val, form: &'static str, schema: FieldSchema) {
        match opts {
            Val::Record(entries) => self.check_record_entry_fields(entries, form, schema),
            _ => {
                let _ = self.infer_val(opts);
            }
        }
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

/// Schema for the `within` options record. `handlers:`/`handler:` hold thunks
/// and dispatch at runtime; `env:` is heterogeneous and `parse_env`'s to judge.
fn within_field_ty(key: &str, _u: &mut Unifier) -> Option<Ty> {
    match key {
        "dir" => Some(Ty::String),
        _ => None,
    }
}

/// Schema for the `grant` options record. Everything but the two flags is
/// `decode_capability_map`'s: a policy value may be a string or a list, and
/// the two mix within one record. Their values are still inferred.
fn grant_field_ty(key: &str, _u: &mut Unifier) -> Option<Ty> {
    match key {
        "net" | "detach" => Some(Ty::Bool),
        _ => None,
    }
}
