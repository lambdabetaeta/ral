//! Typing rules for the `within`, `grant`, `try`, `guard` and `audit` scope
//! nodes.  The sixth scope node, `CompKind::Redirect`, is typed inline in
//! `infer.rs`.
//!
//! A form's options are a closed row, minted fresh at every occurrence from
//! the table it declares in [`super::contract`], and the options value —
//! written out or arriving bound — is unified against it.  One rule, one
//! verdict, both spellings.  `check_declared_row` is that rule, and a contract
//! file's returned row goes through it too: an rc file and a plugin manifest
//! are the same judgment over a different table.
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
use super::contract::{Form, Holds, Key, Table, declared};
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

/// One option a form declares, as this occurrence sees it: the label, the type
/// required there, and the sentence a clash at that label earns.
///
/// Built per occurrence, never shared: two `within`s sharing one row would
/// unite their flags, so `within [dir: "/a"] { }` followed by `within [] { }`
/// would demand that `dir` be present and absent at once.
pub(super) struct FormOption {
    pub(super) label: &'static str,
    pub(super) field: Field,
    pub(super) reason: Reason,
}

/// Mint one occurrence of `table`'s row.  A refused key is declared at a
/// fresh flag over a fresh payload — nothing about its *type* is wrong, so the
/// row absorbs it and the refusal is said in its own words elsewhere.
pub(super) fn occurrence(table: &'static Table, u: &mut Unifier) -> Vec<FormOption> {
    table
        .keys
        .iter()
        .map(|key| {
            let ty = match &key.holds {
                Holds::At(ty) => ty.clone(),
                Holds::Required(ty) => {
                    return FormOption {
                        label: key.label,
                        field: Field::present(ty.clone()),
                        reason: field_reason(table, key),
                    };
                }
                // A refused key is declared like a decoded one: its refusal is
                // about the key, never about the type under it.
                Holds::Decoded | Holds::Refused(_) => u.fresh_ty(),
                Holds::Shaped(shape) => shape(u),
            };
            FormOption {
                label: key.label,
                field: Field::Var(u.fresh_presence_var(), Box::new(ty)),
                reason: field_reason(table, key),
            }
        })
        .collect()
}

fn field_reason(table: &'static Table, key: &Key) -> Reason {
    key.reason.clone().unwrap_or_else(|| Reason::OptionField {
        form: table.form,
        key: key.label.to_string(),
    })
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

impl Inferencer<'_> {
    /// Unify `ty` against one occurrence of `table`'s row.
    ///
    /// Label by label, so a clash carries that label's own sentence, and
    /// closed at the end: a label the table does not declare meets the empty
    /// row there, and the refusal names the table's list.  `written`, when the
    /// value was written out, puts each caret on the entry that earned it.
    pub(super) fn check_declared_row(
        &mut self,
        ty: &Ty,
        table: &'static Table,
        written: Option<&Val>,
    ) {
        let whole = Reason::FormOptions {
            form: table.form,
            options: table.offered(),
        };
        // The declared side goes first throughout, so every verdict reads from
        // the table outwards: a declared type is what was expected, a label
        // the table demands and the row lacks is *missing*, and a label left
        // over when the table is exhausted is *extra*.
        let mut tail = self.ctx.unifier.fresh_row();
        self.ctx
            .unify_ty(&Ty::Record(tail.clone()), ty, whole.clone());
        // A required label is a demand on *membership*, which the row rule
        // states as `Present ~ Absent` — reported from whichever side the
        // fresh-tail rewrite left it on, and that is not reliably this one.
        // So a row already closed is read here and the miss gets its own
        // words; a row still open is left to the rule, which forces it
        // present.  A label missed here is dropped from the declared row, so
        // one absence earns one diagnostic.
        //
        // A refused label is read the same way and for the same reason: the
        // row absorbs it silently, so the advice it carries is said here or
        // not at all.
        let live = self.live_labels(ty);
        let mut missing = Vec::new();
        for key in table.keys {
            let written_out = live
                .as_ref()
                .is_some_and(|ls| ls.iter().any(|l| l == key.label));
            match &key.holds {
                Holds::Required(_) if live.is_some() && !written_out => {
                    missing.push(key.label);
                    self.ctx.diagnose(TypeErrorKind::RowMissingField {
                        label: key.label.to_string(),
                    });
                }
                Holds::Refused(advice) if written_out => {
                    self.ctx.diagnose(TypeErrorKind::RefusedKey {
                        form: table.form,
                        key: key.label,
                        advice,
                    });
                }
                _ => {}
            }
        }
        for FormOption {
            label,
            field,
            reason,
        } in occurrence(table, &mut self.ctx.unifier)
            .into_iter()
            .filter(|o| !missing.contains(&o.label))
        {
            let rest = self.ctx.unifier.fresh_row();
            let declared = Row::Extend(
                Label::Field(label.to_string()),
                field,
                Box::new(rest.clone()),
            );
            // Written out, the caret lands on the entry's own value; a bundle
            // in hand has only the form to point at.
            let at = written.and_then(|opts| written_at(opts, label));
            self.with_span(at, |this| {
                this.ctx
                    .unify_ty(&Ty::Record(declared), &Ty::Record(tail), reason);
            });
            tail = rest;
        }
        self.ctx
            .unify_ty(&Ty::Record(Row::Empty), &Ty::Record(tail), whole);
    }

    /// The field labels `ty`'s row is known to carry, or `None` where it is
    /// not known: a spine ending in a variable still admits any of them.
    fn live_labels(&mut self, ty: &Ty) -> Option<Vec<String>> {
        let Ty::Record(row) = self.ctx.unifier.apply_ty(ty) else {
            return None;
        };
        let mut live = Vec::new();
        let mut rest = row;
        loop {
            match rest {
                Row::Extend(Label::Field(label), field, tail) => {
                    if field.payload().is_some() {
                        live.push(label);
                    }
                    rest = *tail;
                }
                Row::Extend(_, _, tail) => rest = *tail,
                Row::Empty => return Some(live),
                Row::Var(_) => return None,
            }
        }
    }

    /// The options value against the form's own row — written out or arriving
    /// bound, one rule and one verdict.
    fn check_options(&mut self, opts: &Val, table: &'static Table) {
        let opts_ty = self.infer_options_val(opts);
        if matches!(self.ctx.unifier.resolve_ty(&opts_ty), Ty::Map(_)) {
            self.ctx.diagnose(TypeErrorKind::MapAsOptions {
                form: table.form,
                options: table.offered(),
            });
            return;
        }
        self.check_declared_row(&opts_ty, table, Some(opts));
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
        self.check_options(opts, declared(Form::Within));
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
        self.check_options(caps, declared(Form::Grant));
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
