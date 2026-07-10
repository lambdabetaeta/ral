//! Typing rules for the five structural scope nodes.
//!
//! Each rule mirrors the polymorphic scheme the corresponding builtin
//! carries today (`typecheck/builtins.rs`).  Lifting them out is a
//! preparatory step: once the elaborator constructs the scope IR nodes
//! directly, the schemes go away and these rules become the only path.
//!
//! Conventions:
//!
//!   - `try`, `guard`, `audit` leave their body's pipeline modes free: a
//!     control wrapper runs a thunk that may itself read or write bytes
//!     (`try { echo x } …`), so the body is `F[μ₀,μ₁] α`, not `F[none,none]`.
//!     `try` also exposes byte output when either outcome can write bytes, so
//!     a recovery arm such as `{ |_| echo missing }` observes like any other
//!     final byte-output computation.  Pinning the body to `none` would clash
//!     with any byte-output body now that mode unification is equality-strict
//!     (`docs/SPEC.md` §4.2.1).
//!   - `within`, `grant` allocate fresh mode vars and let body's modes
//!     flow up unchanged — the scope is transparent to pipeline I/O.
//!   - Options/capability maps validate their entry shapes against
//!     `within_field_ty` / `grant_field_ty` when the map is a literal;
//!     unknown-key rejection stays at runtime in `WithinScope::parse`.

use super::builtins::{
    FieldSchema, audit_record, grant_field_ty, try_error_record, within_field_ty,
};
use super::infer::Inferencer;
use super::scheme::{Reason, Scheme};
use super::ty::{CompTy, PipeSpec, Ty};
use crate::ir::{Val, ValMapEntry};

impl Inferencer<'_> {
    /// Walk a literal `Val::Map` for `within` opts and collect handler
    /// result schemes from recognisable `handlers:` entries.
    ///
    /// Rules per the design doc:
    ///
    /// - `Entry(String("handlers"), Map(entries))` — for each inner entry whose
    ///   key is a literal `String(name)` and value is a literal `Thunk(comp)`,
    ///   infer the handler under the runtime calling convention, generalise
    ///   the resulting computation result, and collect `(name, scheme)`.
    ///   Non-literal inner keys or values are inferred for side-effects only.
    ///
    /// - `Entry(String("handler"), Thunk(comp))` — catch-all handler: infer
    ///   for side-effects (so type errors inside it surface); no binding, since
    ///   catch-alls match all names and the type system cannot say anything
    ///   specific about them.
    ///
    /// - All other entries (`env`, `dir`, spreads, dynamic keys): not touched
    ///   here — handled by `infer_scope_opts` / `within_field_ty` as before.
    ///
    /// Non-literal `opts` (variable, function call, etc.): return empty.
    fn collect_within_handler_bindings(&mut self, opts: &Val) -> Vec<(String, Scheme)> {
        let Val::Map(outer_entries) = opts else {
            return Vec::new();
        };

        let mut bindings = Vec::new();

        for outer_entry in outer_entries {
            match outer_entry {
                // `handlers: [name: { comp }, ...]`
                ValMapEntry::Entry(Val::String(key), Val::Map(inner_entries))
                    if key == "handlers" =>
                {
                    for inner_entry in inner_entries {
                        match inner_entry {
                            ValMapEntry::Entry(Val::String(name), Val::Thunk(comp)) => {
                                if !self.reject_handler_for_binding(name, "install handler for") {
                                    bindings
                                        .push((name.clone(), self.handler_comp_scheme(name, comp)));
                                }
                            }
                            // Non-literal key or non-thunk value: infer for
                            // side-effects only.  No binding — we can't say
                            // anything specific about a handler we can't see
                            // statically (open question 3 in the design doc).
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

                // `handler: { comp }` — catch-all: infer for side-effects only.
                // Catch-alls match all names, so no specific name can be bound.
                ValMapEntry::Entry(Val::String(key), Val::Thunk(comp)) if key == "handler" => {
                    let _ = self.with_scope(|this| this.infer_comp(comp));
                }

                // `env:`, `dir:`, spreads, dynamic keys — not our concern here.
                // `infer_scope_opts` handles them via `within_field_ty`.
                _ => {}
            }
        }

        bindings
    }

    pub(super) fn infer_within(&mut self, opts: &Val, body: &Val) -> CompTy {
        // Collect static handler bindings from a literal opts map.
        let bindings = self.collect_within_handler_bindings(opts);

        // Validate non-handler entries (env/dir) and infer all values for
        // side-effects, including handler entries a second time.  The second
        // infer of handler thunks is idempotent (unify on already-solved vars
        // is a no-op) and ensures that type errors inside handlers surface
        // even when the handler key isn't statically recognisable.
        self.infer_scope_opts(opts, "within", within_field_ty);

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

        // The handler runs on failure and must produce the same observed value
        // type as the body — `try` yields one or the other.  Its own pipeline
        // modes are independent of the body's, so the handler's result is
        // mode-polymorphic too.
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

    /// Constrain `val` to be `Thunk(F[μ₀,μ₁] α)` for a fresh `α` and fresh
    /// pipeline modes, and return `α`.  The control wrappers (try / guard
    /// body and cleanup / audit body) all take a thunk whose body may have
    /// any pipeline modes — a byte-output body flushes to the visible
    /// stream while the wrapper still returns a value.  This centralises
    /// the unify so the call sites read as 'name the body's result type'.
    fn infer_thunk_body(&mut self, val: &Val) -> Ty {
        match self.infer_scope_body_passthrough(val) {
            CompTy::Return(_, alpha) => *alpha,
            _ => unreachable!("infer_scope_body_passthrough always returns CompTy::Return"),
        }
    }
}
