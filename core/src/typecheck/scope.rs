//! Typing rules for the `within`, `grant`, `try`, `guard` and `audit` scope
//! nodes, plus the field schemas for the `within`/`grant` option maps.  The
//! sixth scope node, `ScopeOp::Redirect`, is typed inline in `infer.rs`.
//!
//! Scope bodies stay mode-polymorphic (`F[μ₀,μ₁] α`) rather than pinned to
//! `F[none,none]`: mode unification is equality-strict, so a pinned `none`
//! would clash with any body that writes bytes (`try { echo x }`).  Unknown
//! option keys are rejected at runtime — by `WithinScope::parse` and
//! `decode_capability_map` — not by the schemas here.
//!
//! No rule here builds a `CompTy` directly: each states which arms run and
//! when, and how the value is observed, and returns a [`ScopeSig`] for
//! [`Inferencer::seal`] to compose into the scope's pipeline modes.

use super::builtins::{FieldSchema, audit_record, try_error_record};
use super::error::Reason;
use super::infer::Inferencer;
use super::scheme::Scheme;
use super::ty::{CompTy, PipeSpec, Ty};
use super::unify::Unifier;
use crate::ir::{Val, ValMapEntry};
use crate::mode::PipeMode;

/// How often a scope arm runs relative to its scope.
enum ArmRuns {
    Always,
    OnFailure,
}

pub(super) struct ScopeArm {
    runs: ArmRuns,
    input: PipeMode,
    output: PipeMode,
}

/// What a scope rule knows before its computation type exists: the arms the
/// evaluator runs, each against the live streams, and the value the scope
/// produces.  Only [`Inferencer::seal`] turns this into a `CompTy`.
pub(super) struct ScopeSig {
    arms: Vec<ScopeArm>,
    value: Ty,
}

impl Inferencer<'_> {
    /// Compose a scope's `PipeSpec` from its arms and seal `sig.value` into a
    /// `CompTy::Return`.  Every arm the evaluator applies runs against the
    /// scope's live streams — capture is a tee, never a silencer — so every
    /// scope is channel-transparent, and this is the only place permitted to
    /// build a scope's computation type.
    ///
    /// Output is bytes-dominant: any arm that may emit bytes makes the scope
    /// byte-emitting, via `join_byte_mode` folded over every arm's output.
    /// Input follows suit only when every arm always runs, since they all
    /// read the same shared stdin; once an arm is `OnFailure`, a clash
    /// between it and an always-arm is not a contradiction but an unknown a
    /// neighbouring stage can pin, so input folds via `union_mode` instead.
    pub(super) fn seal(&mut self, sig: ScopeSig) -> CompTy {
        let all_always = sig
            .arms
            .iter()
            .all(|arm| matches!(arm.runs, ArmRuns::Always));

        let mut arms = sig.arms.into_iter();
        let first = arms.next().expect("a scope always has at least one arm");
        let mut input = first.input;
        let mut output = first.output;

        for arm in arms {
            output = self.join_byte_mode(output, arm.output);
            input = if all_always {
                self.join_byte_mode(input, arm.input)
            } else {
                self.union_mode(input, arm.input)
            };
        }

        CompTy::Return(PipeSpec { input, output }, Box::new(sig.value))
    }
}

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

    pub(super) fn infer_within(&mut self, opts: &Val, body: &Val) -> ScopeSig {
        let bindings = self.infer_within_opts(opts);

        self.env.push();
        for (name, scheme) in bindings {
            self.env.bind_handler(name, scheme, false);
        }
        let body_cty = self.infer_scope_body_passthrough(body);
        self.env.pop();

        let (value, input, output) = self.extract_return(&body_cty);
        ScopeSig {
            arms: vec![ScopeArm {
                runs: ArmRuns::Always,
                input,
                output,
            }],
            value,
        }
    }

    pub(super) fn infer_grant(&mut self, caps: &Val, body: &Val) -> ScopeSig {
        self.infer_scope_opts(caps, "grant", grant_field_ty);
        let body_cty = self.infer_scope_body_passthrough(body);

        let (value, input, output) = self.extract_return(&body_cty);
        ScopeSig {
            arms: vec![ScopeArm {
                runs: ArmRuns::Always,
                input,
                output,
            }],
            value,
        }
    }

    pub(super) fn infer_try(&mut self, body: &Val, handler: &Val) -> ScopeSig {
        let body_cty = self.infer_scope_body_passthrough(body);
        let (body_raw, body_in, body_out) = self.extract_return(&body_cty);
        let body_final_out = self.final_output_of_thunk_value(body, &body_cty);

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

        // Both arms observe under the node's joined final output, not each
        // under its own — the capture mode installed once for the whole scope.
        let joined_out = self.joined_final_output([body_final_out, handler_final_out]);
        let body_value = self.observed_value_ty(body_raw, joined_out);
        let handler_value = self.observed_value_ty(handler_raw, joined_out);
        self.ctx
            .unify_ty(&body_value, &handler_value, Reason::TryArms);

        ScopeSig {
            arms: vec![
                ScopeArm {
                    runs: ArmRuns::Always,
                    input: body_in,
                    output: body_out,
                },
                ScopeArm {
                    runs: ArmRuns::OnFailure,
                    input: handler_in,
                    output: handler_out,
                },
            ],
            value: body_value,
        }
    }

    pub(super) fn infer_guard(&mut self, body: &Val, cleanup: &Val) -> ScopeSig {
        let body_cty = self.infer_scope_body_passthrough(body);
        let (body_raw, body_in, body_out) = self.extract_return(&body_cty);
        let body_final_out = self.final_output_of_thunk_value(body, &body_cty);
        let body_value = self.observed_value_ty(body_raw, body_final_out);

        let cleanup_cty = self.infer_scope_body_passthrough(cleanup);
        let (_cleanup_value, cleanup_in, cleanup_out) = self.extract_return(&cleanup_cty);

        ScopeSig {
            arms: vec![
                ScopeArm {
                    runs: ArmRuns::Always,
                    input: body_in,
                    output: body_out,
                },
                ScopeArm {
                    runs: ArmRuns::Always,
                    input: cleanup_in,
                    output: cleanup_out,
                },
            ],
            value: body_value,
        }
    }

    pub(super) fn infer_audit(&mut self, body: &Val) -> ScopeSig {
        let body_cty = self.infer_scope_body_passthrough(body);
        // The record's `value` field holds the body's raw result — the
        // runtime stores it undecoded.  The body's bytes go to the live
        // stream and to whichever real command nodes wrote them, never to a
        // field of the record itself: `audit` owns no site, so no
        // observation here.
        let (alpha, input, output) = self.extract_return(&body_cty);
        let beta = self.ctx.unifier.fresh_ty();

        ScopeSig {
            arms: vec![ScopeArm {
                runs: ArmRuns::Always,
                input,
                output,
            }],
            value: audit_record(alpha, beta),
        }
    }

    fn infer_scope_opts(&mut self, opts: &Val, form: &'static str, schema: FieldSchema) {
        match opts {
            Val::Map(entries) => self.check_map_entry_fields(entries, form, schema),
            _ => {
                let _ = self.infer_val(opts);
            }
        }
    }

    /// Constrain `body` to `Thunk(F[μ₀,μ₁] α)` for a fresh `α` and fresh
    /// pipeline modes, and return the `F[μ₀,μ₁] α`.
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
