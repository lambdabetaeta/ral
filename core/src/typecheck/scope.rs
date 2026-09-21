//! Typing rules for the `within`, `grant`, `try`, `guard` and `audit` scope
//! nodes, plus the option rows of `within` and `grant`.  The sixth scope node,
//! `CompKind::Redirect`, is typed inline in `infer.rs`.
//!
//! A form's options are a closed row, minted fresh at every occurrence, and
//! the options value — written out or arriving bound — is unified against it.
//! One rule, one verdict, both spellings.
//!
//! The runtime's own unknown-key refusals — `WithinScope::parse`'s and
//! `decode_capability_map`'s — survive that, and are not dead code: `use`
//! *asserts* a module's row rather than checking it, so a module's bindings
//! can still put a key in a bundle that no rule here ever saw.
//!
//! No rule here builds a `CompTy` directly: each states the value a scope
//! produces and the route that carries it, and returns a [`ScopeSig`] for
//! its caller to compose into `CompTy::Return(sig.route, Box::new(sig.value))`.

use super::builtins::{audit_record, try_error_record};
use super::error::{Reason, TypeErrorKind};
use super::infer::Inferencer;
use super::route::PayloadRoute;
use super::scheme::Scheme;
use super::ty::{CompTy, Field, Label, Row, Ty};
use super::unify::Unifier;
use crate::ir::{HandlerArmV, Val, ValRecordEntry};
use crate::source::{Span, WithSpan};

/// What a scope rule knows before its computation type exists: the value the
/// scope produces and the route that carries it.
pub(super) struct ScopeSig {
    pub(super) value: Ty,
    pub(super) route: PayloadRoute,
}

/// One option a form declares: the label, the type required there, and the
/// sentence a clash at that label earns.
///
/// Built per occurrence, never shared: two `within`s sharing one row would
/// unite their flags, so `within [dir: "/a"] { }` followed by `within [] { }`
/// would demand that `dir` be present and absent at once.
struct FormOption {
    label: &'static str,
    ty: Ty,
    reason: Reason,
}

/// `within`'s options.  `env:` is heterogeneous and `parse_env`'s to judge, so
/// it is declared at a variable and stays the decoder's.
///
/// The catch-all declares a route *variable* and pins the value at `Unit`:
/// `bytes_subsumes`' two branches both end in `value ~ Unit` and differ only
/// in whether the route is left at `Value`, so `Unit` is the whole of WF-2's
/// subsumption and leaving the route free is the rest.  Declaring `Bytes`
/// here would commit a bound arm's route one occurrence at a time.
fn within_options(u: &mut Unifier) -> Vec<FormOption> {
    let env = u.fresh_ty();
    let route = u.fresh_route();
    let handler = Ty::Thunk(Box::new(CompTy::Fun(
        Box::new(Ty::String),
        Box::new(CompTy::Fun(
            Box::new(Ty::argv()),
            Box::new(CompTy::Return(route, Box::new(Ty::Unit))),
        )),
    )));
    vec![
        option("within", "dir", Ty::String),
        option("within", "env", env),
        FormOption {
            label: "handler",
            ty: handler,
            reason: Reason::CatchAllRoutePin,
        },
    ]
}

/// `grant`'s options.  Everything but the two flags is
/// `decode_capability_map`'s: a policy value may be a string or a list, and
/// the two mix within one record.  Typing them to full depth would put one
/// label at two ground types — `editor.read` beside `fs.read` — which is the
/// one condition the assignment `Δ` owes.
fn grant_options(u: &mut Unifier) -> Vec<FormOption> {
    ["exec", "fs", "net", "detach", "editor", "shell"]
        .into_iter()
        .map(|label| {
            let ty = match label {
                "net" | "detach" => Ty::Bool,
                _ => u.fresh_ty(),
            };
            option("grant", label, ty)
        })
        .collect()
}

/// Where `label`'s value sits, when the options were written out.
fn written_at(opts: &Val, label: &str) -> Option<Span> {
    let Val::Record(entries) = opts else {
        return None;
    };
    entries.iter().find_map(|entry| match entry {
        ValRecordEntry::Field(key, value) if key == label => value.span,
        ValRecordEntry::Field(..) | ValRecordEntry::Spread(_) => None,
    })
}

fn option(form: &'static str, label: &'static str, ty: Ty) -> FormOption {
    FormOption {
        label,
        ty,
        reason: Reason::OptionField {
            form,
            key: label.to_string(),
        },
    }
}

impl Inferencer<'_> {
    /// Unify the options value against the form's own row — written out or
    /// arriving bound, one rule and one verdict.
    ///
    /// Label by label, so a clash carries that label's own sentence, and
    /// closed at the end: an option the form does not declare meets the empty
    /// row there, and the refusal names the form's list.
    fn check_options(&mut self, opts: &Val, form: &'static str, options: Vec<FormOption>) {
        let opts_ty = self.infer_options_val(opts);
        let labels: Vec<&'static str> = options.iter().map(|o| o.label).collect();
        let whole = Reason::FormOptions {
            form,
            options: labels.clone(),
        };
        if matches!(self.ctx.unifier.resolve_ty(&opts_ty), Ty::Map(_)) {
            self.ctx.diagnose(TypeErrorKind::MapAsOptions {
                form,
                options: labels,
            });
            return;
        }
        let mut tail = self.ctx.unifier.fresh_row();
        self.ctx
            .unify_ty(&opts_ty, &Ty::Record(tail.clone()), whole.clone());
        for FormOption { label, ty, reason } in options {
            let rest = self.ctx.unifier.fresh_row();
            let flag = self.ctx.unifier.fresh_presence_var();
            let declared = Row::Extend(
                Label::Field(label.to_string()),
                Field::Var(flag, Box::new(ty)),
                Box::new(rest.clone()),
            );
            // Written out, the caret lands on the option's own value; a bundle
            // in hand has only the form to point at.
            self.with_span(written_at(opts, label), |this| {
                this.ctx
                    .unify_ty(&Ty::Record(tail), &Ty::Record(declared), reason);
            });
            tail = rest;
        }
        // The empty row first, so what is left over is the *extra* field.
        self.ctx
            .unify_ty(&Ty::Record(Row::Empty), &Ty::Record(tail), whole);
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
        opts: &Val,
        handlers: Option<&[HandlerArmV]>,
        body: &Val,
    ) -> ScopeSig {
        let options = within_options(&mut self.ctx.unifier);
        self.check_options(opts, "within", options);
        let bindings = self.handler_bindings(handlers);

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
        let options = grant_options(&mut self.ctx.unifier);
        self.check_options(caps, "grant", options);
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
