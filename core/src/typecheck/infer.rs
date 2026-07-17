//! Type inference: `infer_val`, `infer_comp`, and supporting helpers.
//!
//! `infer_val` synthesizes a value type (Ty) for a Val node.
//! `infer_comp` synthesizes a computation type (`CompTy`) for a Comp node.
//! Both are mutually recursive: thunk bodies are inferred as computations,
//! and return values are inferred as values.

use super::builtins::{FieldSchema, plugin_entry_field_ty};
use super::env::{InferCtx, TyEnv};
use super::error::{Reason, TypeErrorKind};
use super::generalize::{generalize, instantiate};
use super::scheme::Scheme;
use super::ty::{CompTy, PipeMode, PipeSpec, Row, Ty};
use crate::ir::{
    CommandName, CommandWord, Comp, CompKind, IrPattern, ScopeOp, Val, ValListElem, ValMapEntry,
};
use crate::source::Span;
use crate::source::WithSpan;
use crate::stream::{HEAD_FIELD, TAIL_FIELD, done_tag, more_tag};
use crate::syntax::ast::{BinaryOp, BinaryOpKind};
use crate::syntax::tag::{is_tag_label, tag_row_label};
use std::sync::Arc;

/// Walk a row spine and collect (label, `payload_ty`) pairs in order of first
/// appearance, stopping at the first non-Extend node (Empty or unresolved
/// variable).  Caller is expected to have applied substitutions; a duplicated
/// label resolves last-wins (the deeper, later occurrence's payload type),
/// matching the runtime's last-wins semantics for duplicate keys.
fn collect_extends(row: &Row) -> Vec<(String, Ty)> {
    let mut out: Vec<(String, Ty)> = Vec::new();
    let mut cur = row;
    loop {
        match cur {
            Row::Extend(l, ty, rest) => {
                match out.iter_mut().find(|(k, _)| k == l) {
                    Some(slot) => slot.1 = (**ty).clone(),
                    None => out.push((l.clone(), (**ty).clone())),
                }
                cur = rest;
            }
            _ => return out,
        }
    }
}

/// Best-effort lookup from a `case` arm's row label to the span of
/// the handler body the user wrote at that arm.
///
/// `table` is expected to be the second operand of `case` — a literal
/// `Val::Map` of `(label → handler thunk)` entries.  For each entry
/// whose key is a tag-shaped literal and whose value is a `Val::Thunk`,
/// record `(label_with_leading_backtick, handler_body_span)`.  Any other
/// shape (spread, runtime key, non-thunk handler) is silently skipped
/// — the caller falls back to the enclosing `case` span when no entry
/// matches.
///
/// The "handler body span" peers past the lambda's wrapping `Lam` node
/// (whose span is the enclosing statement, not the body) to the actual
/// body Comp the user wrote.  Without that, `{ |s| body }` arms would
/// resolve back to the enclosing `let`/`case` span and the caret would
/// underline the whole `case` form — exactly what we're trying to
/// avoid.
fn collect_handler_spans(table: &Val) -> std::collections::HashMap<String, crate::source::Span> {
    fn handler_body_span(inner: &Comp) -> Option<crate::source::Span> {
        match &inner.item {
            // `{ |p| body }` elaborates to `Lam { param, body }`; the
            // outer Lam's span is the surrounding statement, but
            // `body.span` is the user-written body itself.
            crate::ir::CompKind::Lam { body, .. } => body.span.or(inner.span),
            _ => inner.span,
        }
    }
    let mut out = std::collections::HashMap::new();
    let Val::Map(entries) = table else {
        return out;
    };
    for entry in entries {
        let crate::ir::ValMapEntry::Entry(key, value) = entry else {
            continue;
        };
        let raw_key = match key {
            Val::String(s) => s.as_str(),
            _ => continue,
        };
        if !is_tag_label(raw_key) {
            continue;
        }
        let Val::Thunk(inner) = value else { continue };
        if let Some(span) = handler_body_span(inner) {
            out.insert(raw_key.to_string(), span);
        }
    }
    out
}

/// Heuristic: did the user almost certainly write a single `"..."` string
/// that the lexer split at an unescaped inner `"`?  Recognised pattern:
/// the head came from a quoted-string source (a `Val::String` or a
/// `CompKind::Interpolation`), AND the args list contains both a string
/// chunk and a hoisted non-string fragment — that's the IR shape the
/// lexer produces when it closes the outer string on an inner `"` and
/// the body in between contains an interpolation / subshell that gets
/// hoisted into its own bind (so it lands as `Val::Variable` in arg
/// position).  Pure `'foo' bar baz` (head-string + bare-word args) keeps
/// the generic hint: every arg is a `Val::String` after [`Val::from_word`]
/// classifies it, so the "non-string fragment" half of the conjunction
/// is false and we fall through.
fn looks_like_nested_quote_mistake(head: &Comp, args: &[&Val]) -> bool {
    let head_from_quoted = matches!(
        head.item,
        CompKind::Return(Val::String(_)) | CompKind::Interpolation(_)
    );
    let any_string_arg = args.iter().any(|a| matches!(a, Val::String(_)));
    let any_non_string_arg = args.iter().any(|a| !matches!(a, Val::String(_)));
    head_from_quoted && any_string_arg && any_non_string_arg
}

/// Recognise the IR shape the elaborator produces for `alias name { body }`.
///
/// Returns `Ok(Some((name, thunk)))` when the shape matches; `Ok(None)`
/// when the head is not `alias` (normal exec, fall through); `Err(msg)`
/// when the head is `alias` but the shape is malformed — a bug in the
/// elaborator or an adversarial IR.
fn alias_statement_shape(part: &Comp) -> Result<Option<(&str, &Arc<Comp>)>, &'static str> {
    let CompKind::Exec(exec) = &part.item else {
        return Ok(None);
    };
    let CommandWord::Name(CommandName::Bare(head)) = &exec.head else {
        return Ok(None);
    };
    if head != "alias" {
        return Ok(None);
    }
    if !exec.redirects.is_empty() {
        return Err("alias: redirects in alias definition are not allowed");
    }
    let Some(positional) = crate::ir::args::positional(&exec.args) else {
        return Err("alias: spread arguments in alias definition are not allowed");
    };
    let [Val::String(name), Val::Thunk(thunk)] = positional[..] else {
        return Err("alias: expected `alias name { body }`");
    };
    Ok(Some((name.as_str(), thunk)))
}

fn unalias_statement_shape(part: &Comp) -> Result<Option<&str>, &'static str> {
    let CompKind::Exec(exec) = &part.item else {
        return Ok(None);
    };
    let CommandWord::Name(CommandName::Bare(head)) = &exec.head else {
        return Ok(None);
    };
    if head != "unalias" {
        return Ok(None);
    }
    if !exec.redirects.is_empty() {
        return Err("unalias: redirects are not allowed");
    }
    let Some(positional) = crate::ir::args::positional(&exec.args) else {
        return Err("unalias: spread arguments are not allowed");
    };
    let [Val::String(name)] = positional[..] else {
        return Err("unalias: expected `unalias name`");
    };
    Ok(Some(name.as_str()))
}

pub fn infer_comp(ctx: &mut InferCtx, env: &mut TyEnv, comp: &Comp) -> CompTy {
    Inferencer { ctx, env }.infer_comp(comp)
}

/// Inference state.  Both the struct and its fields are `pub(super)` —
/// only code inside `typecheck/` can name it or read/mutate its fields.
pub(super) struct Inferencer<'a> {
    pub(super) ctx: &'a mut InferCtx,
    pub(super) env: &'a mut TyEnv,
}

impl WithSpan for Inferencer<'_> {
    fn span_slot(&mut self) -> &mut Option<Span> {
        &mut self.ctx.pos
    }
}

impl Inferencer<'_> {
    pub(super) fn with_scope<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved_pos = self.ctx.pos;
        self.env.push();
        let out = f(self);
        self.env.pop();
        self.ctx.pos = saved_pos;
        out
    }

    fn bind_pattern(&mut self, pat: &IrPattern, ty: &Ty) {
        match pat {
            IrPattern::Wildcard => {}
            IrPattern::Name(name) => {
                self.env.bind(name.clone(), Scheme::mono(ty.clone()));
            }
            IrPattern::List { elems, rest } => {
                let elem = self.ctx.unifier.fresh_ty();
                self.ctx
                    .unify_ty(ty, &Ty::List(Box::new(elem.clone())), Reason::ListPattern);
                for elem_pat in elems {
                    self.bind_pattern(elem_pat, &elem);
                }
                if let Some(rest_name) = rest {
                    self.env
                        .bind(rest_name.clone(), Scheme::mono(Ty::List(Box::new(elem))));
                }
            }
            IrPattern::Map(entries) => {
                // Required entries (no default) shape the value's row;
                // defaulted entries do not — the field may be absent and
                // the default supplies the binding instead.  The field's
                // type stays a fresh tyvar in either case, refined by
                // uses of the bound name.
                let tail = self.ctx.unifier.fresh_row_var();
                let mut row = Row::Var(tail);
                let mut field_tys = Vec::with_capacity(entries.len());
                for entry in entries.iter().rev() {
                    let field_ty = self.ctx.unifier.fresh_ty();
                    field_tys.push(field_ty.clone());
                    if entry.default.is_none() {
                        row = Row::Extend(entry.key.row_label(), Box::new(field_ty), Box::new(row));
                    }
                }
                field_tys.reverse();
                self.ctx
                    .unify_ty(ty, &Ty::Record(row), Reason::RecordPattern);
                for (entry, field_ty) in entries.iter().zip(field_tys.iter()) {
                    self.bind_pattern(&entry.pattern, field_ty);
                }
            }
        }
    }

    pub(super) fn extract_return(&mut self, cty: &CompTy) -> (Ty, PipeMode, PipeMode) {
        if let CompTy::Return(spec, ty) = self.ctx.unifier.resolve_comp_ty(cty) {
            (*ty, spec.input, spec.output)
        } else {
            let ty = self.ctx.unifier.fresh_ty();
            let input = self.ctx.unifier.fresh_mode();
            let output = self.ctx.unifier.fresh_mode();
            let expected = CompTy::Return(PipeSpec { input, output }, Box::new(ty.clone()));
            self.ctx.unify_comp_ty(cty, &expected, Reason::ReturnShape);
            (ty, input, output)
        }
    }

    /// Value observed when a byte-output computation crosses a value boundary.
    ///
    /// Most computations carry their real value directly.  A byte-output
    /// computation whose value is `Unit` is the one intentional exception:
    /// the bytes are the value at `let`/force-like boundaries, decoded as a
    /// `String` by the evaluator after stripping one trailing newline.
    pub(super) fn observed_value_ty(&mut self, ty: Ty, output: PipeMode) -> Ty {
        match (
            self.ctx.unifier.resolve_mode(&output),
            self.ctx.unifier.resolve_ty(&ty),
        ) {
            (PipeMode::Bytes, Ty::Unit) => Ty::String,
            _ => ty,
        }
    }

    /// Single-step force of a producer's return type at a value edge.
    ///
    /// A bare-block producer stage (`{ … }`) has return type
    /// `Ty::Thunk(body)` — the suspended body computation.  At a value
    /// edge the runtime forces it exactly once (`run_value_fold` runs
    /// the block, yielding the body's result), so the value that
    /// crosses to the consumer is the *body's* return
    /// type.  Deref one thunk level to mirror that; a non-thunk producer
    /// value (a `List`, a `Return`-typed stage's concrete value, or a
    /// free var the consumer will constrain) passes through unchanged so
    /// existing value-edge shapes (`[1,2,3] | { |xs| … }`) are untouched.
    fn deref_forced_producer(&mut self, ty: Ty) -> Ty {
        match self.ctx.unifier.resolve_ty(&ty) {
            Ty::Thunk(inner) => match self.ctx.unifier.resolve_comp_ty(&inner) {
                CompTy::Return(_, inner_ty) => *inner_ty,
                // A `Fun`-shaped thunk (a `{ |x| … }` lambda producer with
                // no upstream) is itself the produced value — forcing a
                // lambda yields the lambda, matching `step_force`.  Leave
                // the thunk in place so the consumer sees the function.
                _ => ty,
            },
            _ => ty,
        }
    }

    /// Project the I/O end (input or output) of a computation type, peering
    /// past `Fun` arrows.  An unresolved comp var yields a fresh mode.
    fn comp_end_mode(&mut self, cty: &CompTy, pick: fn(PipeSpec) -> PipeMode) -> PipeMode {
        match self.ctx.unifier.resolve_comp_ty(cty) {
            CompTy::Return(spec, _) => pick(spec),
            CompTy::Fun(_, body) => self.comp_end_mode(&body, pick),
            CompTy::Var(_) => self.ctx.unifier.fresh_mode(),
        }
    }

    /// A stage's own channel signature for the annotation pass.
    ///
    /// A stage consumed as a value argument (the data-last fold's
    /// function) takes its upstream on the value edge, so its input
    /// channel is `∅`; its output is whatever the application result
    /// emits — `infer_pipeline` has already rewritten the stage's type to
    /// that result, so `comp_output_mode` reads it directly (a `{ |x|
    /// echo $x }` consumer emits `Bytes`).  Otherwise both channel modes
    /// come from the stage's [`PipeSpec`], not peering past `Fun` arrows.
    fn stage_own_spec(&mut self, cty: &CompTy, consumed_as_value: bool) -> PipeSpec {
        if consumed_as_value {
            return PipeSpec {
                input: PipeMode::None,
                output: self.comp_output_mode(cty),
            };
        }
        PipeSpec {
            input: self.comp_input_mode(cty),
            output: self.comp_output_mode(cty),
        }
    }

    fn comp_input_mode(&mut self, cty: &CompTy) -> PipeMode {
        self.comp_end_mode(cty, |s| s.input)
    }

    fn comp_output_mode(&mut self, cty: &CompTy) -> PipeMode {
        self.comp_end_mode(cty, |s| s.output)
    }

    /// Union of two branch modes: exactly one branch runs, so a clash is
    /// not a contradiction but an unknown — the conditional's mode is
    /// then a fresh variable a downstream stage can pin.  Agreement (or a
    /// variable on either side) unifies as usual.  A conditional that
    /// emits bytes in one arm and a value in the other (`if c { echo x }
    /// else {}`) is accepted rather than rejected.
    pub(super) fn union_mode(&mut self, a: PipeMode, b: PipeMode) -> PipeMode {
        if self.ctx.unifier.unify_mode(&a, &b).is_err() {
            self.ctx.unifier.fresh_mode()
        } else {
            a
        }
    }

    pub(super) fn join_byte_output(&mut self, a: PipeMode, b: PipeMode) -> PipeMode {
        match (
            self.ctx.unifier.resolve_mode(&a),
            self.ctx.unifier.resolve_mode(&b),
        ) {
            (PipeMode::Bytes, _) | (_, PipeMode::Bytes) => PipeMode::Bytes,
            (PipeMode::None, PipeMode::None) => PipeMode::None,
            _ => self.union_mode(a, b),
        }
    }

    /// Merge a conditional's branches into one computation type.  The
    /// return value type is shared — every branch must produce the same
    /// value (`unify_ty` reports a real disagreement) — but the
    /// pipeline I/O modes are *unioned* via [`Self::union_mode`], not
    /// equated, since only one branch runs.  A non-`Return` branch (a
    /// bare lambda arm) falls back to strict computation-type unification.
    fn merge_branches(&mut self, branches: Vec<CompTy>, why: &Reason) -> CompTy {
        let mut iter = branches.into_iter();
        let Some(mut acc) = iter.next() else {
            return CompTy::pure(self.ctx.unifier.fresh_ty());
        };
        for branch in iter {
            acc = if let (CompTy::Return(sa, ta), CompTy::Return(sb, tb)) = (
                self.ctx.unifier.resolve_comp_ty(&acc),
                self.ctx.unifier.resolve_comp_ty(&branch),
            ) {
                self.ctx.unify_ty(&ta, &tb, why.clone());
                CompTy::Return(
                    PipeSpec {
                        input: self.union_mode(sa.input, sb.input),
                        output: self.union_mode(sa.output, sb.output),
                    },
                    ta,
                )
            } else {
                self.ctx.unify_comp_ty(&acc, &branch, why.clone());
                acc
            };
        }
        acc
    }

    /// Eventual return type of a computation, peering past `Fun` arrows
    /// the same way [`Self::comp_end_mode`] peers past them for modes.
    /// An unresolved `Var` yields a fresh type — callers that need the
    /// constraint propagated back must unify themselves.
    fn comp_return_ty(&mut self, cty: &CompTy) -> Ty {
        match self.ctx.unifier.resolve_comp_ty(cty) {
            CompTy::Return(_, ty) => *ty,
            CompTy::Fun(_, body) => self.comp_return_ty(&body),
            CompTy::Var(_) => self.ctx.unifier.fresh_ty(),
        }
    }

    /// Does this pipeline consumer take its upstream as a *value
    /// argument* rather than over the byte channel?  A consumer applied
    /// to the piped value is function-shaped — a bare lambda (`Fun`), a
    /// block literal carrying one (`Return(_, Thunk(Fun))`), or a head
    /// still unknown enough to become one (a `Var`).  A consumer that is
    /// a concrete non-thunk stage (`Return(_, τ)` for a byte decoder like
    /// `from-X`) reads its input over the channel, so a value producer
    /// feeding it is a `∅`-into-`Bytes` channel adjacency, not an
    /// application.  A stage whose input mode resolves to ground `Bytes`
    /// concretely reads the byte channel, so it takes the channel edge
    /// regardless of how polymorphic its return value is.  Peers past
    /// block-literal thunks the same way [`Self::autoderef_thunk_return`]
    /// does, without planting constraints.
    fn consumes_value_arg(&mut self, cty: &CompTy) -> bool {
        match self.ctx.unifier.resolve_comp_ty(cty) {
            CompTy::Fun(..) | CompTy::Var(_) => true,
            CompTy::Return(spec, ty) => {
                if self.ctx.unifier.resolve_mode(&spec.input) == PipeMode::Bytes {
                    return false;
                }
                match self.ctx.unifier.resolve_ty(&ty) {
                    Ty::Thunk(inner) => self.consumes_value_arg(&inner),
                    Ty::Var(_) => true,
                    _ => false,
                }
            }
        }
    }

    fn autoderef_thunk_return(&mut self, mut cty: CompTy) -> CompTy {
        loop {
            match self.ctx.unifier.resolve_comp_ty(&cty) {
                CompTy::Return(_, ty) => match self.ctx.unifier.resolve_ty(&ty) {
                    Ty::Thunk(inner) => cty = *inner,
                    // Free type variable in head position: this matches the
                    // runtime behavior where eval_app's Thunk arm trampoline-
                    // forces a Thunk value before applying args.  At type
                    // level we constrain the head to be a Thunk and continue
                    // unfolding.  Without this, a parameter `$f` whose type
                    // is yet unknown would fail to unify when args are
                    // applied.
                    Ty::Var(_) => {
                        let inner = self.ctx.unifier.fresh_comp_ty();
                        self.ctx.unify_ty(
                            &ty,
                            &Ty::Thunk(Box::new(inner.clone())),
                            Reason::AutoderefHead,
                        );
                        cty = inner;
                    }
                    _ => return cty,
                },
                _ => return cty,
            }
        }
    }

    pub(super) fn apply_args(&mut self, mut cty: CompTy, args: &crate::ir::Args) -> CompTy {
        // Precise per-arg checking is only possible when the args list
        // has no spreads.  With spread the arity is dynamic; we still
        // infer sub-expressions for type errors inside them, but don't
        // constrain the function's parameter list.
        let Some(positional) = crate::ir::args::positional(args) else {
            for sub in crate::ir::args::iter_subvals(args) {
                let _ = self.infer_val(sub);
            }
            return cty;
        };
        for (i, arg) in positional.into_iter().enumerate() {
            cty = self.autoderef_thunk_return(cty);
            // Narrow pos to this argument's source range so a per-arg
            // unify failure underlines the offending argument rather
            // than the whole call.  Synthetic entries (no span,
            // hoisted applications) fall back to the call's own pos
            // via `with_span`'s `None`-as-no-op branch.
            cty = self.with_span(args[i].span, |this| {
                let arg_ty = this.infer_val(arg);
                let result = this.ctx.unifier.fresh_comp_ty();
                let expected = CompTy::Fun(Box::new(arg_ty), Box::new(result.clone()));
                this.ctx.unify_comp_ty(&cty, &expected, Reason::Argument);
                result
            });
        }
        cty
    }

    /// If `head_ty` resolves to a `Return(_, ty)` where `ty` is concretely
    /// not a function — i.e. not a `Thunk` and not a free type variable that
    /// could later become one — return that `ty`.  Otherwise return `None`.
    ///
    /// Used by `CompKind::App` to detect `'foo' bar baz` and friends and
    /// raise a surface-level diagnostic before the general unifier
    /// mismatch fires.
    fn command_non_function_ty(&mut self, head_ty: &CompTy) -> Option<Ty> {
        match self.ctx.unifier.resolve_comp_ty(head_ty) {
            CompTy::Return(_, ty) => match self.ctx.unifier.resolve_ty(&ty) {
                Ty::Thunk(_) | Ty::Var(_) => None,
                concrete => Some(concrete),
            },
            CompTy::Fun(_, _) | CompTy::Var(_) => None,
        }
    }

    fn apply_piped_value(&mut self, cty: CompTy, piped_ty: &Ty) -> CompTy {
        let cty = self.autoderef_thunk_return(cty);
        let result = self.ctx.unifier.fresh_comp_ty();
        let expected = CompTy::Fun(Box::new(piped_ty.clone()), Box::new(result.clone()));
        // The consumer is a value-arg function (the caller routes only
        // `Fun`/`Var` consumers here; a `Return` stage takes the channel
        // edge instead), so the produced value must fit its first
        // parameter.  A clash here is a genuine arg-shape error.
        let step_stream = self.piped_ty_is_step_shaped(piped_ty);
        self.ctx
            .unify_comp_ty(&cty, &expected, Reason::PipedValue { step_stream });
        result
    }

    /// Does the piped value's type resolve to a variant whose row carries
    /// a Step label (`` `more `` / `` `done ``)?  Diagnostic-only: the
    /// answer shapes the unification hint in [`Self::apply_piped_value`],
    /// never the types.
    fn piped_ty_is_step_shaped(&mut self, ty: &Ty) -> bool {
        let Ty::Variant(row) = self.ctx.unifier.apply_ty(ty) else {
            return false;
        };
        let more_tag = more_tag();
        let done_tag = done_tag();
        collect_extends(&row)
            .iter()
            .any(|(l, _)| l == &more_tag || l == &done_tag)
    }

    /// The value shape returned by `from-lines`: a recursive Step stream
    /// of Strings, i.e. `` `more {head: String, tail: Thunk(F Step)}`` or
    /// `` `done ``.  The recursion closes through a comp-var root, not a `TyVar`.
    pub(super) fn lines_step_ty(&mut self) -> Ty {
        let tail_comp = self.ctx.unifier.fresh_comp_ty();
        let more_tag = more_tag();
        let done_tag = done_tag();
        let payload = Ty::Record(Row::Extend(
            HEAD_FIELD.into(),
            Box::new(Ty::String),
            Box::new(Row::Extend(
                TAIL_FIELD.into(),
                Box::new(Ty::Thunk(Box::new(tail_comp.clone()))),
                Box::new(Row::Empty),
            )),
        ));
        let step = Ty::Variant(Row::Extend(
            more_tag,
            Box::new(payload),
            Box::new(Row::Extend(
                done_tag,
                Box::new(Ty::Unit),
                Box::new(Row::Empty),
            )),
        ));
        self.ctx.unify_comp_ty(
            &tail_comp,
            &CompTy::pure(step.clone()),
            Reason::LinesStepSelf,
        );
        step
    }

    /// Validate a map literal's entries against a per-key `schema`.
    ///
    /// For each entry, the value is inferred (so side-effects and inner
    /// type errors surface); additionally, if the key is a literal the
    /// `schema` knows, the value's type is unified against the expected
    /// one.  Unknown keys, spreads, and dynamic keys stay runtime-
    /// dispatched.  Shared by `within`, `grant`, and rc plugin entries —
    /// three shapes of the same "optional-args map" idiom.
    pub(super) fn check_map_entry_fields(
        &mut self,
        entries: &[ValMapEntry],
        form: &'static str,
        schema: FieldSchema,
    ) {
        for entry in entries {
            let (key, val) = match entry {
                ValMapEntry::Entry(Val::String(k), v) => (Some(k.as_str()), v),
                ValMapEntry::Entry(_, v) | ValMapEntry::Spread(v) => (None, v),
            };
            let expected = key.and_then(|k| schema(k, &mut self.ctx.unifier));
            let actual = self.infer_val(val);
            if let (Some(key), Some(expected)) = (key, expected) {
                self.ctx.unify_ty(
                    &actual,
                    &expected,
                    Reason::OptionField {
                        form,
                        key: key.to_string(),
                    },
                );
            }
        }
    }

    /// Infer an rc `plugins:` list: validate each literal-map entry against
    /// the plugin-entry schema, with no cross-entry unification so entries
    /// with mixed shapes coexist.  The list's element type is a fresh var.
    fn infer_plugins_list(&mut self, elems: &[ValListElem]) -> Ty {
        for elem in elems {
            match elem {
                ValListElem::Single(Val::Map(entries)) => {
                    self.check_map_entry_fields(entries, "plugin entry", plugin_entry_field_ty);
                }
                ValListElem::Single(v) => {
                    let _ = self.infer_val(v);
                }
                ValListElem::Spread(v) => {
                    let spread_ty = self.infer_val(v);
                    let inner = self.ctx.unifier.fresh_ty();
                    self.ctx
                        .unify_ty(&spread_ty, &Ty::List(Box::new(inner)), Reason::ListSpread);
                }
            }
        }
        Ty::List(Box::new(self.ctx.unifier.fresh_ty()))
    }

    /// Instantiate `scheme` and apply the resulting body to a positional
    /// `args` list.  Strips the outer `Thunk`, then runs `apply_args`.
    /// Used by every entry-point dispatcher that lands on a scheme — the
    /// registry's `Scheme` rule and the host-scheme fallback.  Going
    /// through `instantiate()` prevents quantifier-var sharing across call
    /// sites, so callers can pass a `Scheme` reference directly without
    /// instantiating first.  For mono schemes `instantiate` short-circuits
    /// to a clone of the body, so there is no overhead in the common case.
    pub(super) fn apply_scheme(
        &mut self,
        scheme: &super::scheme::Scheme,
        args: &crate::ir::Args,
    ) -> CompTy {
        let head_cty = self.instantiate_comp(scheme);
        self.apply_args(head_cty, args)
    }

    /// Infer the arm for head `name`, pin its `PipeSpec` to the head's, and
    /// generalise.  Reinterpreting a known head preserves that head's modes;
    /// an unknown head's modes are whatever the arm defines.  The arm's value
    /// type stays whatever inference yields.  Reinterpreting a known head with
    /// incompatible modes surfaces as a positioned
    /// [`TypeErrorKind::ModeMismatch`] (`docs/SPEC.md` §4.2.1).
    pub(super) fn handler_comp_scheme(&mut self, name: &str, comp: &Comp) -> Scheme {
        let cty = self.infer_handler_comp(comp);
        if let Err(mismatch) = self.pin_arm_to_head(name, &cty) {
            self.ctx.report(
                TypeErrorKind::ModeMismatch {
                    expected: mismatch.left,
                    actual: mismatch.right,
                },
                Reason::HandlerModePin,
            );
        }
        let thunk_ty = Ty::Thunk(Box::new(cty));
        super::generalize::generalize(&mut self.ctx.unifier, self.env, &thunk_ty)
    }

    /// Head `name`'s known `PipeSpec`: the spec carried by its handler
    /// scheme when one is already in scope, so reinterpreting a known head
    /// constrains the arm to that head's modes.  An unknown head carries no
    /// spec, so it yields a fully fresh `F[μ, ν]` and the arm defines the
    /// head's modes; the byte-channel discipline is enforced where pipeline
    /// channels connect (`docs/SPEC.md` §4.2.1).  A scheme that does not
    /// resolve to a `Return` constrains nothing, so it too yields a fully
    /// fresh spec.  Lexical bindings and builtins never reach here — the
    /// install guards reject those names before any arm is inferred.
    fn head_pipe_spec(&mut self, name: &str) -> PipeSpec {
        let spec = self.env.lookup_handler(name).cloned().and_then(|handler| {
            let cty = self.instantiate_comp(&handler.scheme);
            match self.alias_arm_body(&cty) {
                CompTy::Return(spec, _) => Some(spec),
                _ => None,
            }
        });
        spec.unwrap_or_else(|| self.ctx.unifier.fresh_spec())
    }

    /// Peel the leading `Fun` arrows of an alias arm to reach the body's
    /// computation.  An arm with a parameter is typed `Fun(argv, body)`
    /// (the calling convention forces it on the argv list); the head's
    /// pipeline modes live on the *body's* `Return`, so mode pinning works
    /// past the parameter arrows.
    fn alias_arm_body(&mut self, cty: &CompTy) -> CompTy {
        match self.ctx.unifier.resolve_comp_ty(cty) {
            CompTy::Fun(_, body) => self.alias_arm_body(&body),
            resolved => resolved,
        }
    }

    /// Unify the arm's `PipeSpec` against head `name`'s known spec, leaving
    /// the arm's value type free.  Forcing the arm's `CompTy` to a `Return`
    /// shape is infallible; the only failure is a ground mode clash, returned
    /// distinctly so the install path can reject it while the static path
    /// positions it.
    pub(super) fn pin_arm_to_head(
        &mut self,
        name: &str,
        arm: &CompTy,
    ) -> Result<(), crate::mode::ModeMismatch> {
        let body = self.alias_arm_body(arm);
        let (_, arm_input, arm_output) = self.extract_return(&body);
        let head = self.head_pipe_spec(name);
        self.ctx.unifier.unify_mode(&arm_input, &head.input)?;
        self.ctx.unifier.unify_mode(&arm_output, &head.output)
    }

    /// Instantiate `scheme` and strip the outer `Thunk` that schemes
    /// carry, yielding the bare computation type — a fresh comp var if
    /// the instantiated body is not a thunk.
    fn instantiate_comp(&mut self, scheme: &Scheme) -> CompTy {
        match instantiate(&mut self.ctx.unifier, scheme) {
            Ty::Thunk(body) => *body,
            _ => self.ctx.unifier.fresh_comp_ty(),
        }
    }

    /// Apply an alias/handler arm to a call site's arguments.
    ///
    /// A parameterised arm is typed `Fun(List(elem), body)`: the calling
    /// convention forces it on the argv *list*, so every supplied argument
    /// must inhabit the arm's element type `elem`.  Unifying each argument
    /// against `elem` connects the call site to the arm's parameter —
    /// without it an arm whose body constrains `elem` (e.g. `$[$a[0] + 1]`
    /// pinning `elem` to `Integer`) accepted any argument and deferred the
    /// clash to runtime.  Spreads splice a whole list, so each is unified
    /// against `List(elem)`.  A nullary arm carries no parameter; its
    /// arguments are inferred for inner errors but discarded, matching the
    /// runtime, which ignores them.
    fn apply_alias_arm(&mut self, scheme: &Scheme, args: &crate::ir::Args) -> CompTy {
        let cty = self.instantiate_comp(scheme);
        let CompTy::Fun(param, body) = self.ctx.unifier.resolve_comp_ty(&cty) else {
            self.infer_args(args);
            return cty;
        };
        let elem = self.ctx.unifier.fresh_ty();
        self.ctx.unify_ty(
            &param,
            &Ty::List(Box::new(elem.clone())),
            Reason::AliasParam,
        );
        for entry in args {
            let span = entry.span;
            match &entry.item {
                crate::ir::ValListElem::Single(arg) => {
                    self.with_span(span, |this| {
                        let arg_ty = this.infer_val(arg);
                        this.ctx.unify_ty(&arg_ty, &elem, Reason::AliasArgv);
                    });
                }
                crate::ir::ValListElem::Spread(arg) => {
                    let spread_ty = self.infer_val(arg);
                    self.ctx.unify_ty(
                        &spread_ty,
                        &Ty::List(Box::new(elem.clone())),
                        Reason::ListSpread,
                    );
                }
            }
        }
        *body
    }

    /// The runtime handler calling convention: an alias arm is forced on
    /// the argv list.  When `param` is `Some`, the arm is a lambda whose
    /// parameter binds the argv (`Ty::List` of a fresh element type)
    /// inside a fresh scope; the arm's type keeps its `Fun(argv, body)`
    /// shape so the call site can unify the supplied arguments against the
    /// parameter type ([`Self::apply_alias_arm`]).  When `None`, the arm is
    /// a bare body inferred inside that same scope frame with no argument
    /// bound.
    pub(super) fn infer_alias_arm(&mut self, param: Option<&IrPattern>, body: &Comp) -> CompTy {
        match param {
            Some(param) => {
                let elem = self.ctx.unifier.fresh_ty();
                let argv_ty = Ty::List(Box::new(elem));
                let body_cty = self.with_scope(|this| {
                    this.bind_pattern(param, &argv_ty);
                    this.infer_comp(body)
                });
                CompTy::Fun(Box::new(argv_ty), Box::new(body_cty))
            }
            None => self.with_scope(|this| this.infer_comp(body)),
        }
    }

    /// The ordinary calling convention for a value binding installed as a
    /// lexical scope binding: a lambda is a function `Fun(param, body)`
    /// whose parameter binds a fresh value type (independent per
    /// parameter) inside a fresh scope, a block is its bare body inferred
    /// in that same scope frame.
    pub(super) fn infer_binding_value(&mut self, param: Option<&IrPattern>, body: &Comp) -> CompTy {
        match param {
            Some(param) => {
                let param_ty = self.ctx.unifier.fresh_ty();
                let body_ty = self.with_scope(|this| {
                    this.bind_pattern(param, &param_ty);
                    this.infer_comp(body)
                });
                CompTy::Fun(Box::new(param_ty), Box::new(body_ty))
            }
            None => self.with_scope(|this| this.infer_comp(body)),
        }
    }

    fn infer_handler_comp(&mut self, comp: &Comp) -> CompTy {
        match &comp.item {
            CompKind::Lam { param, body } => self.infer_alias_arm(Some(param), body),
            _ => self.infer_alias_arm(None, comp),
        }
    }

    pub(super) fn binding_claims_name(&self, name: &str) -> bool {
        self.env.lookup_binding(name).is_some() || self.env.builtins.get(name).is_some()
    }

    pub(super) fn reject_handler_for_binding(&mut self, name: &str, verb: &'static str) -> bool {
        if !self.binding_claims_name(name) {
            return false;
        }
        let kind = if self.env.builtins.get(name).is_some() {
            TypeErrorKind::CannotRedefineBuiltin {
                name: name.to_string(),
                verb,
            }
        } else {
            TypeErrorKind::HandlerShadowedByBinding {
                name: name.to_string(),
            }
        };
        self.ctx.diagnose(kind);
        true
    }

    fn infer_not(&mut self, val: &Val) -> Ty {
        let ty = self.infer_val(val);
        self.ctx.unify_ty(&ty, &Ty::Bool, Reason::NotOperand);
        Ty::Bool
    }

    fn infer_binary(&mut self, op: BinaryOp, lhs: &Val, rhs: &Val) -> Ty {
        let lhs_ty = self.infer_val(lhs);
        let rhs_ty = self.infer_val(rhs);
        self.ctx
            .unify_ty(&lhs_ty, &rhs_ty, Reason::BinaryOperands(op.kind()));
        match op.kind() {
            BinaryOpKind::Eq(_) | BinaryOpKind::Compare(_) => Ty::Bool,
            BinaryOpKind::Arith(_) => lhs_ty,
        }
    }

    fn exec_comp_ty(&mut self, name: &str, args: &crate::ir::Args, external_only: bool) -> CompTy {
        // Binding first: lexical/prelude names beat handlers and external
        // commands.  A binding hit is final; errors in callability do not
        // fall through to shell-style command lookup.
        if !external_only && let Some(scheme) = self.env.lookup_binding(name).cloned() {
            return self.apply_scheme(&scheme, args);
        }

        // Builtin binding.  These are language names, not user handlers,
        // so they are consulted before aliases and `within [handlers:]`.
        if !external_only && let Some(entry) = self.env.builtins.get(name) {
            use super::builtins::BuiltinTypeRule;
            match entry.type_rule {
                BuiltinTypeRule::Scheme(_, factory) => {
                    let scheme = factory(&mut self.ctx.unifier);
                    return self.apply_scheme(&scheme, args);
                }
                BuiltinTypeRule::Sig(sig) => return self.apply_builtin_sig(sig, args),
            }
        }

        if let Some(handler) = self.env.lookup_handler(name).cloned() {
            return self.apply_alias_arm(&handler.scheme, args);
        }

        // Fallback: a name that is neither a binding, a builtin, nor a
        // handler is an external command.  Prelude functions reach the
        // checker as bound variables (`App`), never as a bare `Exec` head,
        // so no internal classification is needed here.
        self.external_exec_comp_ty(args)
    }

    fn external_exec_comp_ty(&mut self, args: &crate::ir::Args) -> CompTy {
        self.infer_args(args);
        let input = self.ctx.unifier.fresh_mode();
        CompTy::Return(
            PipeSpec {
                input,
                output: PipeMode::Bytes,
            },
            Box::new(Ty::String),
        )
    }

    pub(super) fn infer_args(&mut self, args: &crate::ir::Args) {
        for sub in crate::ir::args::iter_subvals(args) {
            let _ = self.infer_val(sub);
        }
    }

    /// Walk a `Seq`'s statements in order, binding alias definitions into
    /// the current `TyEnv` scope as they are encountered so subsequent
    /// statements in the same Seq can resolve against them.  Always runs
    /// normal inference on every statement afterwards — the binding is
    /// additive, not a replacement, so errors inside the alias body still
    /// surface through the alias builtin's own type rule.
    ///
    /// The Seq must run inside a `with_scope` frame (added at the caller
    /// in `infer_comp`'s `Seq` arm) so the bindings do not leak past the
    /// Seq's lexical extent.  Aliases inside conditional or function
    /// bodies aren't at Seq level, so they don't leak — documented
    /// behaviour.
    ///
    /// An alias whose thunk is not a literal lambda is still bound, typed
    /// by its body — `handler_comp_scheme` falls to the nullary arm when
    /// the thunk IR is not a `Lam` (e.g. a computed thunk `alias g $h`).
    /// Binding it means `g x` is a static arity mismatch rather than a
    /// silently discarded `x`.  A bare-block alias (`alias g { ... }`) is
    /// rejected at runtime install — the canonical form is `{ |args| … }`
    /// — but the static layer stays lenient here, since a thunk's runtime
    /// value is not known statically.
    pub(super) fn infer_seq_with_alias_bindings(
        &mut self,
        parts: &[Arc<Comp>],
        empty: Ty,
    ) -> CompTy {
        let mut last = CompTy::pure(empty);
        let mut emits_bytes = false;
        for part in parts {
            let mut alias_already_typed = false;
            match alias_statement_shape(part) {
                Ok(Some((name, thunk))) => {
                    if !self.reject_handler_for_binding(name, "alias") {
                        let scheme = self.handler_comp_scheme(name, thunk);
                        self.env.bind_handler(name.to_string(), scheme, true);
                        alias_already_typed = true;
                    }
                }
                Err(msg) => {
                    self.ctx
                        .diagnose(TypeErrorKind::MalformedAlias { detail: msg });
                }
                Ok(None) => {}
            }
            match unalias_statement_shape(part) {
                Ok(Some(name)) => {
                    self.env.unbind_removable_handler(name);
                }
                Err(msg) => {
                    self.ctx
                        .diagnose(TypeErrorKind::MalformedUnalias { detail: msg });
                }
                Ok(None) => {}
            }
            // The alias body was already inferred above via
            // `handler_comp_scheme`, which is the sole authority for its
            // type and has already emitted any diagnostics. Falling
            // through to `infer_comp` would dispatch the same
            // `Exec("alias", …)` through `sig::ALIAS`, re-inferring the
            // identical thunk body and duplicating every diagnostic
            // inside it. The statement's own type is the `ALIAS` builtin's
            // fixed pure-Unit result, so synthesize that instead.
            last = if alias_already_typed {
                super::builtins::pure(Ty::Unit)
            } else {
                self.infer_comp(part)
            };
            let out = self.comp_output_mode(&last);
            emits_bytes |= self.ctx.unifier.resolve_mode(&out) == PipeMode::Bytes;
        }
        self.lift_seq_output(last, emits_bytes)
    }

    /// A `Seq`'s stdout is everything its statements write, so its
    /// byte-output mode is a join over the sequence: `Bytes` if *any*
    /// statement emits bytes, not merely the last.  The return value and
    /// input mode stay the last statement's — only the output mode is
    /// lifted, so a byte-emitting body (e.g. the `map-lines`/`filter-lines`
    /// callbacks that `echo` per line) classifies as byte-output.  A
    /// `Fun`-tailed sequence is a block that yields a function, not a
    /// pipeline stage, so it keeps its shape.
    fn lift_seq_output(&mut self, last: CompTy, emits_bytes: bool) -> CompTy {
        if !emits_bytes {
            return last;
        }
        if let CompTy::Fun(..) = self.ctx.unifier.resolve_comp_ty(&last) {
            last
        } else {
            let (ret, input, _) = self.extract_return(&last);
            CompTy::Return(
                PipeSpec {
                    input,
                    output: PipeMode::Bytes,
                },
                Box::new(ret),
            )
        }
    }

    /// Infer a `Chain`: `a ? b ? c …` returns whichever arm succeeds
    /// at runtime, so the chain's overall *value type* can be any of
    /// the arms' return types — a union we don't have a precise way
    /// to spell.  Typecheck each arm independently for errors within
    /// it, but expose the chain's return type as a fresh variable so
    /// downstream consumers don't accidentally pin themselves to one
    /// arm's choice and silently miscompute when another arm wins.
    /// (Previous code returned the *last* arm's type, which is
    /// unsound: `(return 1) ? (return "hi")` was typed `String` even
    /// though the value at runtime was `1: Int`.)
    ///
    /// The pipeline I/O modes, by contrast, are *unioned* across the
    /// arms via [`Self::union_mode`] — exactly as [`Self::merge_branches`]
    /// does for a conditional — since only one arm runs: `tmux a ? tmux b`
    /// emits bytes whichever arm wins, so the chain is byte-output rather
    /// than the value edge `CompTy::pure` would force.
    fn infer_chain(&mut self, parts: &[Arc<Comp>]) -> CompTy {
        let arm_specs: Vec<PipeSpec> = parts
            .iter()
            .map(|part| {
                let arm = self.infer_comp(part);
                match self.ctx.unifier.resolve_comp_ty(&arm) {
                    CompTy::Return(s, _) => s,
                    _ => PipeSpec::none(),
                }
            })
            .collect();
        let mut specs = arm_specs.into_iter();
        let mut spec = specs.next().unwrap_or_else(PipeSpec::none);
        for arm_spec in specs {
            spec = PipeSpec {
                input: self.union_mode(spec.input, arm_spec.input),
                output: self.union_mode(spec.output, arm_spec.output),
            };
        }
        CompTy::Return(spec, Box::new(self.ctx.unifier.fresh_ty()))
    }

    fn infer_map_val(&mut self, entries: &[ValMapEntry]) -> Ty {
        let all_literal_keys = entries.iter().all(|entry| match entry {
            ValMapEntry::Entry(Val::String(_), _) | ValMapEntry::Spread(_) => true,
            ValMapEntry::Entry(_, _) => false,
        });

        if all_literal_keys && !entries.is_empty() {
            // `all_literal_keys` rules out non-string keys for every
            // `Entry`, so the match below is exhaustive without a
            // wildcard `Entry(_, _)` arm.
            let mut spread_rows = Vec::new();
            let mut field_entries = Vec::new();
            for entry in entries {
                match entry {
                    ValMapEntry::Entry(Val::String(key), value)
                        if key == "plugins" && matches!(value, Val::List(_)) =>
                    {
                        let Val::List(elems) = value else {
                            unreachable!("guard restricts value to Val::List(_)")
                        };
                        let ty = self.infer_plugins_list(elems);
                        field_entries.push((key.clone(), ty));
                    }
                    ValMapEntry::Entry(Val::String(key), value) => {
                        field_entries.push((key.clone(), self.infer_val(value)));
                    }
                    ValMapEntry::Spread(value) => {
                        let spread_ty = self.infer_val(value);
                        let row_var = self.ctx.unifier.fresh_row_var();
                        self.ctx.unify_ty(
                            &spread_ty,
                            &Ty::Record(Row::Var(row_var)),
                            Reason::MapSpread,
                        );
                        spread_rows.push(row_var);
                    }
                    ValMapEntry::Entry(_, _) => {
                        unreachable!("all_literal_keys guarantees every Entry has a String key")
                    }
                }
            }

            // A duplicate explicit key resolves last-wins, matching the
            // runtime `Value::map` (`[x: 1, x: 2]` denotes `[x: 2]`).  Keep
            // each label's final value type; first-appearance order is
            // retained only to give the row spine a deterministic shape.
            let mut deduped: Vec<(String, Ty)> = Vec::new();
            for (key, value_ty) in field_entries {
                match deduped.iter_mut().find(|(k, _)| *k == key) {
                    Some(slot) => slot.1 = value_ty,
                    None => deduped.push((key, value_ty)),
                }
            }

            let mut row = match spread_rows.len() {
                0 => Row::Empty,
                1 => Row::Var(spread_rows[0]),
                _ => Row::Var(self.ctx.unifier.fresh_row_var()),
            };
            for (key, value_ty) in deduped.into_iter().rev() {
                row = Row::Extend(key, Box::new(value_ty), Box::new(row));
            }
            Ty::Record(row)
        } else {
            // Dynamic-key map (`[$k: v, …]`) — the type is `Map<elem>`,
            // where `elem` is shared by every entry's value and every
            // spread's element type.  Unifying all of them rules out the
            // silent-mistyping shape: without the constraint a literal
            // like `[$k: 1, $j: "hi"]` would check as `Map<α>` with α
            // free, leaving the consumer free to pick (e.g. `Map<Int>`)
            // while a String still sits at one key.
            //
            // Every dynamic key must itself be a `String` — the runtime
            // rejects non-string keys with a sigil-1 error, so we lift
            // that check up to typecheck time.  A literal like `[2: foo]`
            // is an Int-vs-String type error rather than a deferred
            // runtime failure.
            let elem = self.ctx.unifier.fresh_ty();
            for entry in entries {
                match entry {
                    ValMapEntry::Entry(key, value) => {
                        let key_ty = self.infer_val(key);
                        self.ctx.unify_ty(&key_ty, &Ty::String, Reason::MapKey);
                        let value_ty = self.infer_val(value);
                        self.ctx.unify_ty(&value_ty, &elem, Reason::MapElem);
                    }
                    ValMapEntry::Spread(value) => {
                        let spread_ty = self.infer_val(value);
                        self.ctx.unify_ty(
                            &spread_ty,
                            &Ty::Map(Box::new(elem.clone())),
                            Reason::MapSpread,
                        );
                    }
                }
            }
            Ty::Map(Box::new(elem))
        }
    }

    pub(super) fn infer_val(&mut self, val: &Val) -> Ty {
        match val {
            Val::Unit => Ty::Unit,
            Val::TildePath(_) | Val::String(_) => Ty::String,
            Val::Int(_) => Ty::Int,
            Val::Float(_) => Ty::Float,
            Val::Bool(_) => Ty::Bool,
            Val::Variable(name) => {
                if matches!(
                    name.as_str(),
                    "within" | "try" | "guard" | "grant" | "audit"
                ) {
                    self.ctx
                        .diagnose(TypeErrorKind::ControlOperatorAsValue { name: name.clone() });
                    self.ctx.unifier.fresh_ty()
                } else {
                    match self.env.lookup_binding(name).cloned() {
                        Some(scheme) => instantiate(&mut self.ctx.unifier, &scheme),
                        None => {
                            match super::builtins::builtin_scheme(
                                &self.env.builtins,
                                name,
                                &mut self.ctx.unifier,
                            ) {
                                Some(scheme) => instantiate(&mut self.ctx.unifier, &scheme),
                                None if self.env.lookup_handler(name).is_some() => {
                                    self.ctx.diagnose(TypeErrorKind::HandlerNotFirstClass {
                                        name: name.clone(),
                                    });
                                    self.ctx.unifier.fresh_ty()
                                }
                                None if self.env.builtins.get(name).is_some() => {
                                    self.ctx.diagnose(TypeErrorKind::BuiltinNotFirstClass {
                                        name: name.clone(),
                                    });
                                    self.ctx.unifier.fresh_ty()
                                }
                                None => self.ctx.unifier.fresh_ty(),
                            }
                        }
                    }
                }
            }
            Val::Thunk(comp) => Ty::Thunk(Box::new(self.with_scope(|this| this.infer_comp(comp)))),
            Val::List(elems) => {
                let elem = self.ctx.unifier.fresh_ty();
                for entry in elems {
                    let entry_ty = match entry {
                        ValListElem::Single(value) => self.infer_val(value),
                        ValListElem::Spread(value) => {
                            let spread_ty = self.infer_val(value);
                            let inner = self.ctx.unifier.fresh_ty();
                            self.ctx.unify_ty(
                                &spread_ty,
                                &Ty::List(Box::new(inner.clone())),
                                Reason::ListSpread,
                            );
                            inner
                        }
                    };
                    self.ctx.unify_ty(&entry_ty, &elem, Reason::ListElem);
                }
                Ty::List(Box::new(elem))
            }
            Val::Map(entries) => self.infer_map_val(entries),
            Val::Variant { label, payload } => {
                // Variant construction is open: `` `ok 5 `` infers
                // [`ok: Int | ρ] where ρ is a fresh row variable.  The
                // label is stored *with* its leading backtick in the row so that
                // alphabet checks at unify time treat it as a tag.
                let payload_ty = match payload {
                    Some(p) => self.infer_val(p),
                    None => Ty::Unit,
                };
                let rest = self.ctx.unifier.fresh_row();
                Ty::Variant(Row::Extend(
                    tag_row_label(label),
                    Box::new(payload_ty),
                    Box::new(rest),
                ))
            }
        }
    }

    fn infer_pipeline(&mut self, stages: &[Arc<Comp>]) -> CompTy {
        // The parser guarantees ≥2 stages: a single-stage parse is
        // unwrapped to the bare stage and never becomes Ast::Pipeline,
        // and the elaborator preserves that shape.
        debug_assert!(stages.len() >= 2, "Pipeline carries ≥2 stages");

        let mut stage_tys: Vec<CompTy> =
            stages.iter().map(|stage| self.infer_comp(stage)).collect();
        // A stage consumed as a value argument is the data-last fold's
        // function: its input channel is `∅` (the upstream arrives as the
        // final argument, not over a byte pipe), while its output channel
        // is whatever its application body emits — a `{ |x| echo $x }`
        // consumer is a `Bytes` producer for the stage after it.  The
        // rewrite below replaces such a stage's type with its application
        // body; record the value-input fact now so the annotation pass
        // and the pipeline's tail output mode read input `∅` paired with
        // the applied body's output.
        let mut consumed_as_value = vec![false; stage_tys.len()];
        for i in 0..stage_tys.len() - 1 {
            // A clash on this edge underlines its consumer stage (the stage
            // that fails to accept the upstream), falling back to the
            // producer.  Without this narrowing the diagnostic carries
            // whatever `ctx.pos` the last-inferred stage left behind, so a
            // clash on an early edge would underline the final stage.
            let edge_span = stages[i + 1].span.or(stages[i].span);
            let out = self.comp_output_mode(&stage_tys[i]);
            let out_resolved = self.ctx.unifier.resolve_mode(&out);

            // A value-producing stage (`∅` output) feeding a value-arg
            // function consumer is data-last application: `x | f` is
            // `f !{x}`, and `apply_piped_value` flows the produced value
            // into the function's first parameter.  A
            // value producer feeding a non-application stage (a concrete
            // `Return`, e.g. a `from-X` byte decoder) is a plain channel
            // edge: unify the modes so a `∅`-into-`Bytes` adjacency is
            // rejected as the §4.2.1 mismatch it is, rather than forced
            // through the function path.  `consumes_value_arg` peers past
            // block-literal thunks so a `{ |v| … }` consumer still takes
            // the application path.
            //
            // A still-unresolved output mode is the diverging-producer
            // case (`{ fail … }`, whose `fail` carries fresh, quantified
            // channel modes): it grounds to `∅` (value edge) per the
            // `Var → Empty` grounding rule, so when the consumer takes a
            // value arg it is a value edge too.  Treat it like `None`
            // here, otherwise the producer's *thunk* value type is
            // unified against the consumer's parameter (e.g. `{Command α}`
            // vs `Int`) instead of forcing the producer and piping its
            // return type — the runtime would then hand the consumer the
            // unforced producer block rather than running it.
            let out_is_value_edge = matches!(out_resolved, PipeMode::None | PipeMode::Var(_));
            if out_is_value_edge && self.consumes_value_arg(&stage_tys[i + 1]) {
                let (piped_ty, _, _) = self.extract_return(&stage_tys[i]);
                // A bare-block producer (`{ … }`) is a `Return(_,
                // Thunk(body))`: the runtime forces it once before the
                // value crosses the edge (see `run_value_fold`), so the
                // piped value is the *body's*
                // return type, not the thunk itself.  Deref one thunk
                // level to mirror that single force — without this a
                // `{ fail … } | { |v| … }` producer pipes `{Command α}`
                // into the consumer's parameter, clashing with whatever
                // concrete type the consumer expects.
                let piped_ty = self.deref_forced_producer(piped_ty);
                let next = stage_tys[i + 1].clone();
                stage_tys[i + 1] =
                    self.with_span(edge_span, |this| this.apply_piped_value(next, &piped_ty));
                consumed_as_value[i + 1] = true;
                continue;
            }

            let inp = self.comp_input_mode(&stage_tys[i + 1]);
            self.with_span(edge_span, |this| {
                this.ctx.unify_mode(&out, &inp, Reason::PipelineEdge);
            });
        }

        // Pipeline shape: input from the first stage, output mode and
        // return type from the last.  `Fun` at the tail is the
        // byte-pipe-to-value-arg case (e.g. `cat foo | length`): the
        // typechecker doesn't yet model that connection structurally,
        // so `comp_return_ty` drills past the arrows the same way
        // `comp_output_mode` does for modes.  For a `Var`-resolved
        // last we then unify it back against the synthesized `Return`
        // shape, so consumers of the pipeline see the actual return
        // type rather than an unrelated fresh variable.
        let input = self.comp_input_mode(&stage_tys[0]);
        let last_consumed = consumed_as_value[stage_tys.len() - 1];
        let last = stage_tys
            .last()
            .expect("≥2 stages by invariant above")
            .clone();
        // The tail's contribution to the pipeline's output channel: a
        // value-arg-consumed last stage emits no bytes on its own channel,
        // so the pipeline is a value producer there.
        let output = self.stage_own_spec(&last, last_consumed).output;
        let ret_ty = self.comp_return_ty(&last);
        if matches!(self.ctx.unifier.resolve_comp_ty(&last), CompTy::Var(_)) {
            let bound = CompTy::Return(
                PipeSpec {
                    input: self.comp_input_mode(&last),
                    output,
                },
                Box::new(ret_ty.clone()),
            );
            self.ctx.unify_comp_ty(&last, &bound, Reason::ReturnShape);
        }

        // Record each stage's byte channels and value type for the
        // annotation pass.  The modes and type vars may still be
        // unresolved; they resolve once the whole walk's constraints are in.
        for (i, (stage, ty)) in stages.iter().zip(&stage_tys).enumerate() {
            let spec = self.stage_own_spec(ty, consumed_as_value[i]);
            let value_ty = self.comp_return_ty(ty);
            let key = std::ptr::from_ref::<Comp>(stage.as_ref()) as usize;
            self.ctx.stage_specs.insert(key, spec);
            self.ctx.stage_types.insert(key, value_ty);
        }

        CompTy::Return(PipeSpec { input, output }, Box::new(ret_ty))
    }

    fn infer_index(&mut self, target: &Val, keys: &[crate::source::Spanned<Val>]) -> CompTy {
        let mut current_ty = self.infer_val(target);
        for key in keys {
            current_ty = self.with_span(key.span, |this| {
                this.infer_index_step(&current_ty, &key.item)
            });
        }
        CompTy::pure(current_ty)
    }

    /// One step of an indexing chain — `current_ty[key]`.  Runs under
    /// the caller's narrowed pos (`infer_index` wraps each call in
    /// `with_span`) so any unify failure here underlines just this
    /// step rather than the whole chain.
    fn infer_index_step(&mut self, current_ty: &Ty, key: &Val) -> Ty {
        let resolved = self.ctx.unifier.apply_ty(current_ty);
        match resolved {
            Ty::List(elem) => {
                // List index: the key must be `Int`, so unifying it
                // against `Ty::Int` rejects `xs["foo"]` on a `[_]`.
                let key_ty = self.infer_val(key);
                self.ctx.unify_ty(&key_ty, &Ty::Int, Reason::ListIndexKey);
                *elem
            }
            Ty::Map(elem) => {
                // Map index: key must be `String`.  Same shape of
                // discarded-key-type unsoundness as the List case
                // above.
                let key_ty = self.infer_val(key);
                self.ctx.unify_ty(&key_ty, &Ty::String, Reason::MapIndexKey);
                *elem
            }
            Ty::Thunk(_) => {
                self.ctx.diagnose(TypeErrorKind::IndexIntoThunk);
                let _ = self.infer_val(key);
                self.ctx.unifier.fresh_ty()
            }
            _ => {
                // A statically-known string key (quoted or bare-non-numeric
                // after `Val::from_word` classification) reads a record
                // field; bare-numeric `Val::Int` keys flow through the
                // dynamic-key arm below and reject with the "can't index
                // …" hint (no record uses an Int field name).
                let record_label = match key {
                    Val::String(label) => Some(label.clone()),
                    _ => None,
                };
                if let Some(label) = record_label {
                    let field_ty = self.ctx.unifier.fresh_ty();
                    let tail_row = self.ctx.unifier.fresh_row();
                    let record_ty = Ty::Record(Row::Extend(
                        label.clone(),
                        Box::new(field_ty.clone()),
                        Box::new(tail_row),
                    ));
                    // When the target is concretely *not* a record
                    // (Int, String, Bool, …), the raw unify error
                    // surfaces `Int vs [b: α, ...ρ]` — accurate but
                    // hostile.  Catch the concrete case and produce a
                    // sentence the user can act on.
                    let resolved = self.ctx.unifier.apply_ty(current_ty);
                    let concretely_non_record = !matches!(resolved, Ty::Record(_) | Ty::Var(_));
                    if concretely_non_record {
                        self.ctx.diagnose(TypeErrorKind::FieldOnNonRecord {
                            label,
                            ty: resolved,
                        });
                    } else {
                        self.ctx
                            .unify_ty(current_ty, &record_ty, Reason::RecordFieldRead);
                    }
                    field_ty
                } else {
                    // Dynamic-key index on a non-List/Map/Thunk
                    // target.  Catching this case is what makes
                    // `let x = 42; $x[$k]` a typecheck error rather
                    // than a deferred runtime failure.
                    //
                    // For a free target, use the key's type to pin
                    // it: `Int` ⇒ `List<elem>`, `String` ⇒
                    // `Map<elem>`.  Otherwise leave it; whatever pins
                    // it later will run through the List/Map arms
                    // above and unify the key correctly.
                    //
                    // For a target whose shape is already known and
                    // isn't `List` / `Map` / `Thunk`, raise a
                    // typecheck error — no value of that shape
                    // accepts dynamic indexing.
                    let key_ty = self.infer_val(key);
                    let elem = self.ctx.unifier.fresh_ty();
                    let resolved_target = self.ctx.unifier.apply_ty(current_ty);
                    match resolved_target {
                        Ty::Var(_) => match self.ctx.unifier.apply_ty(&key_ty) {
                            Ty::Int => self.ctx.unify_ty(
                                current_ty,
                                &Ty::List(Box::new(elem.clone())),
                                Reason::DynamicIndexTarget,
                            ),
                            Ty::String => self.ctx.unify_ty(
                                current_ty,
                                &Ty::Map(Box::new(elem.clone())),
                                Reason::DynamicIndexTarget,
                            ),
                            _ => {}
                        },
                        other => {
                            self.ctx
                                .diagnose(TypeErrorKind::DynamicIndexOnScalar { ty: other });
                        }
                    }
                    elem
                }
            }
        }
    }

    /// Check one `case` arm against the case form's `result_cty` and the
    /// scrutinee's resolved per-label payload row `scrut_payloads`.
    fn check_case_arm(
        &mut self,
        label: &str,
        handler_ty: &Ty,
        result_cty: &CompTy,
        scrut_payloads: &std::collections::HashMap<String, Ty>,
    ) -> Ty {
        let payload_ty = self.ctx.unifier.fresh_ty();
        let expected = Ty::Thunk(Box::new(CompTy::Fun(
            Box::new(payload_ty.clone()),
            Box::new(result_cty.clone()),
        )));
        if self.ctx.unifier.unify_ty(handler_ty, &expected).is_err() {
            let expected_resolved = self.ctx.unifier.apply_ty(&expected);
            let found_resolved = self.ctx.unifier.apply_ty(handler_ty);
            self.ctx.diagnose(TypeErrorKind::CaseLabelTypeMismatch {
                label: label.to_string(),
                expected: expected_resolved,
                found: found_resolved,
            });
        }
        // If the scrutinee already has a known payload at this label
        // (e.g. the scrutinee was inferred from a literal `\`ok 5`),
        // force the handler's payload to agree with it here, while
        // pos is on the arm.  Without this the mismatch only surfaces
        // in the final row-unify, where pos has been restored and the
        // caret lands on the entire `case` form.
        if let Some(scrut_payload) = scrut_payloads.get(label)
            && self
                .ctx
                .unifier
                .unify_ty(&payload_ty, scrut_payload)
                .is_err()
        {
            let expected_resolved = self.ctx.unifier.apply_ty(scrut_payload);
            let found_resolved = self.ctx.unifier.apply_ty(&payload_ty);
            self.ctx.report(
                TypeErrorKind::TyMismatch {
                    expected: expected_resolved,
                    actual: found_resolved,
                },
                Reason::CaseArmPayload,
            );
            return self.ctx.unifier.fresh_ty();
        }
        payload_ty
    }

    fn infer_case(
        &mut self,
        scrutinee: &crate::source::Spanned<Val>,
        table: &crate::source::Spanned<Val>,
    ) -> CompTy {
        // Scrutinee is a variant value; table is a record-of-thunks value.
        // CBPV: `case` is the eliminator over a sum value with a record of
        // continuations — both operands sit in value position.
        let scrutinee_span = scrutinee.span;
        let table_span = table.span;
        let scrut_ty = self.with_span(scrutinee_span, |this| this.infer_val(&scrutinee.item));
        let table_ty = self.with_span(table_span, |this| this.infer_val(&table.item));
        let result_cty = self.ctx.unifier.fresh_comp_ty();

        // Pre-build a lookup from handler label → that handler's inner
        // Comp span (the body of the thunk the user wrote at this arm).
        // When a per-arm payload-mismatch fires, we point the caret at
        // *that arm* rather than at the whole `case` form — `let r =
        // case x [\`ok: { … }, \`err: { … }]` is far too wide a target
        // for what is conceptually a single-arm complaint.
        let handler_spans = collect_handler_spans(&table.item);

        // Shape constraints.  If the scrutinee is already concretely
        // *not* a variant (Int, String, Record, …), prefer a friendly
        // "case needs a variant" diagnostic over the raw row-shape
        // mismatch — the latter prints `[...ρ]` which a beginner has
        // no way to read.
        let scrut_resolved = self.ctx.unifier.apply_ty(&scrut_ty);
        let scrut_row_var = self.ctx.unifier.fresh_row_var();
        self.with_span(scrutinee_span, |this| match scrut_resolved {
            Ty::Variant(_) | Ty::Var(_) => {
                this.ctx.unify_ty(
                    &scrut_ty,
                    &Ty::Variant(Row::Var(scrut_row_var)),
                    Reason::CaseScrutinee,
                );
            }
            other => {
                this.ctx
                    .diagnose(TypeErrorKind::CaseOnNonVariant { ty: other });
            }
        });
        let handler_row_var = self.ctx.unifier.fresh_row_var();
        self.with_span(table_span, |this| {
            this.ctx.unify_ty(
                &table_ty,
                &Ty::Record(Row::Var(handler_row_var)),
                Reason::CaseTable,
            );
        });

        // Resolve the handler row.  Record literals always close to Empty,
        // so this returns a clean label list under normal use.
        let handler_resolved = self.ctx.unifier.apply_row(&Row::Var(handler_row_var));
        let handler_labels = collect_extends(&handler_resolved);

        // Pre-resolve the scrutinee's per-label payload types so the
        // per-arm loop can unify each handler's payload against its
        // matching scrut payload *under the arm's own pos*.  Anything
        // here that's still a Var was contributed by the handlers and
        // is fine to leave for the final row-unify pass.
        let scrut_resolved_row = self.ctx.unifier.apply_row(&Row::Var(scrut_row_var));
        let scrut_payloads: std::collections::HashMap<String, Ty> =
            collect_extends(&scrut_resolved_row).into_iter().collect();

        // Per-label connection: each handler at `.l` must be a thunk of
        // a function `payload_l → result_cty`.  Build the closed
        // scrutinee row from these payload types as we go.
        let mut closed_scrut = Row::Empty;
        for (label, handler_ty) in handler_labels.iter().rev() {
            // Narrow pos to *this arm's* body for the duration of the
            // per-arm work, so an arm-local error (handler shape,
            // payload type, return-type disagreement) underlines that
            // arm rather than the entire `case` form.
            let arm_span = handler_spans.get(label.as_str()).copied();
            let closed_payload = self.with_span(arm_span, |this| {
                this.check_case_arm(label, handler_ty, &result_cty, &scrut_payloads)
            });
            closed_scrut = Row::Extend(
                label.clone(),
                Box::new(closed_payload),
                Box::new(closed_scrut),
            );
        }

        // Force scrutinee row to exactly the handler label set.  Row mismatch
        // becomes CaseNotExhaustive: an extra label on the handler side means
        // the handler covers a constructor the scrutinee can never produce;
        // a missing label means the scrutinee has a constructor with no arm.
        //
        // But when the handler row is still a bare variable — the table came
        // from a lambda parameter or an unchecked name rather than a record
        // literal — the handler set is *unknown*, not missing. Closing it to
        // Empty and unifying would falsely report every scrutinee tag as
        // uncovered. Defer to runtime, matching the checker's existing
        // leniency for unbound names.
        if !matches!(handler_resolved, Row::Var(_))
            && let Err(kind) = self
                .ctx
                .unifier
                .unify_row(&Row::Var(scrut_row_var), &closed_scrut)
        {
            use crate::typecheck::error::TypeErrorKind;
            let translated = match kind {
                TypeErrorKind::RowExtraField { label } => TypeErrorKind::CaseNotExhaustive {
                    missing: vec![],
                    extra: vec![label],
                },
                TypeErrorKind::RowMissingField { label } => TypeErrorKind::CaseNotExhaustive {
                    missing: vec![label],
                    extra: vec![],
                },
                other => other,
            };
            self.ctx.diagnose(translated);
        }

        result_cty
    }

    /// Establish each binding as a self-referential mono thunk, then infer
    /// every RHS in that recursive environment, unifying it against its own
    /// thunk type.  Returns the per-binding computation types (`betaᵢ`) — the
    /// shared core of both `LetRec` arms.  The caller decides what to do with
    /// the still-installed mono self-bindings: `slot: None` generalises and
    /// rebinds them into the current scope; `slot: Some(i)` infers inside a
    /// throwaway scope and returns binding `i`'s type.
    fn infer_letrec_betas(&mut self, bindings: &[(String, Val)]) -> Vec<CompTy> {
        let betas: Vec<CompTy> = bindings
            .iter()
            .map(|_| self.ctx.unifier.fresh_comp_ty())
            .collect();

        for ((name, _), beta) in bindings.iter().zip(betas.iter()) {
            self.env.bind(
                name.clone(),
                Scheme::mono(Ty::Thunk(Box::new(beta.clone()))),
            );
        }
        for ((_, lam_val), beta) in bindings.iter().zip(betas.iter()) {
            let lam_ty = self.infer_val(lam_val);
            self.ctx.unify_ty(
                &lam_ty,
                &Ty::Thunk(Box::new(beta.clone())),
                Reason::LetRecSelf,
            );
        }
        betas
    }

    /// `LetRec { slot: Some(i) }` re-establishes the group in a throwaway
    /// scope and returns binding `i`'s lambda.  Infer the whole group inside a
    /// `with_scope` frame so its type errors surface and the self-bindings do
    /// not leak, and yield binding `i`'s thunk type as the produced value.
    /// These nodes are synthesised by `eval_letrec` at runtime, so the path is
    /// normally exercised only when such IR is re-checked; inferring the
    /// bodies keeps it sound rather than returning an unconstrained fresh var.
    fn infer_letrec_slot(&mut self, bindings: &[(String, Val)], slot: usize) -> CompTy {
        self.with_scope(|this| {
            let betas = this.infer_letrec_betas(bindings);
            let beta = betas
                .get(slot)
                .cloned()
                .unwrap_or_else(|| this.ctx.unifier.fresh_comp_ty());
            CompTy::pure(Ty::Thunk(Box::new(beta)))
        })
    }

    fn infer_letrec(&mut self, bindings: &[(String, Val)]) -> CompTy {
        let betas = self.infer_letrec_betas(bindings);
        // Drop the mono self-bindings before generalising.  If they
        // stayed in env, `env_free_vars` would see their (post-body)
        // free comp/ty/row vars as residuals and `generalize` would
        // refuse to quantify them — which silently un-poly's every
        // recursive scheme and lets one call site bind a polymorphic
        // var that all other call sites then share.  Re-bind below
        // with the polymorphic schemes once each is built.
        for (name, _) in bindings {
            self.env.unbind(name);
        }
        let mut schemes: Vec<(String, Scheme)> = Vec::with_capacity(bindings.len());
        for ((name, _), beta) in bindings.iter().zip(betas.iter()) {
            let thunk_ty = Ty::Thunk(Box::new(beta.clone()));
            let scheme = generalize(&mut self.ctx.unifier, self.env, &thunk_ty);
            schemes.push((name.clone(), scheme));
        }
        for (name, scheme) in schemes {
            self.env.bind(name, scheme);
        }

        CompTy::pure(Ty::Unit)
    }

    pub(super) fn final_output_of_comp(&mut self, comp: &Comp, fallback: &CompTy) -> PipeMode {
        self.ctx
            .final_outputs
            .get(&(std::ptr::from_ref::<Comp>(comp) as usize))
            .copied()
            .unwrap_or_else(|| self.comp_output_mode(fallback))
    }

    pub(super) fn final_output_of_thunk_value(&mut self, val: &Val, fallback: &CompTy) -> PipeMode {
        match val {
            Val::Thunk(comp) => self.final_output_of_comp(comp, fallback),
            _ => self.comp_output_mode(fallback),
        }
    }

    fn record_final_output(&mut self, comp: &Comp, cty: &CompTy) {
        let final_output = match &comp.item {
            CompKind::Seq(parts) => parts
                .last()
                .and_then(|last| {
                    self.ctx
                        .final_outputs
                        .get(&(std::ptr::from_ref::<Comp>(last.as_ref()) as usize))
                        .copied()
                })
                .unwrap_or(PipeMode::None),
            CompKind::Bind { rest, .. } => self.final_output_of_comp(rest, cty),
            CompKind::Lam { body, .. }
            | CompKind::Force(Val::Thunk(body))
            | CompKind::Scope(ScopeOp::Redirect { body, .. }) => self.final_output_of_comp(body, cty),
            CompKind::If { then, else_, .. } => {
                let then_out = self.final_output_of_comp(then, cty);
                let else_out = self.final_output_of_comp(else_, cty);
                self.join_byte_output(then_out, else_out)
            }
            CompKind::Chain(parts) => parts.iter().fold(PipeMode::None, |acc, part| {
                let out = self.final_output_of_comp(part, cty);
                self.join_byte_output(acc, out)
            }),
            CompKind::Scope(
                ScopeOp::Within { body, .. }
                | ScopeOp::Grant { body, .. }
                | ScopeOp::Guard { body, .. },
            ) => self.final_output_of_thunk_value(body, cty),
            CompKind::Scope(ScopeOp::Try { body, handler }) => {
                let body_out = self.final_output_of_thunk_value(body, cty);
                let handler_out = self.final_output_of_thunk_value(handler, cty);
                self.join_byte_output(body_out, handler_out)
            }
            _ => self.comp_output_mode(cty),
        };
        self.ctx
            .final_outputs
            .insert(std::ptr::from_ref::<Comp>(comp) as usize, final_output);
    }

    pub(super) fn infer_comp(&mut self, comp: &Comp) -> CompTy {
        // Update position from the node's span.
        if let Some(span) = comp.span {
            self.ctx.pos = Some(span);
        }

        let cty = match &comp.item {
            CompKind::Return(value) => CompTy::pure(self.infer_val(value)),
            CompKind::Lam { param, body } => self.infer_binding_value(Some(param), body),
            CompKind::Force(value) => {
                let val_ty = self.infer_val(value);
                let cty = self.ctx.unifier.fresh_comp_ty();
                self.ctx.unify_ty(
                    &val_ty,
                    &Ty::Thunk(Box::new(cty.clone())),
                    Reason::ForceOperand,
                );
                cty
            }
            CompKind::Bind {
                comp: inner,
                pattern,
                rest,
                ..
            } => {
                let inner_ty = self.infer_comp(inner);
                // A `Fun` RHS is a lambda: evaluating it builds a closure
                // and emits no bytes, so its output channel is `∅`.  Any
                // other shape carries its `Return` spec's output mode.
                //
                // At a byte-output let boundary, `Unit` means "the bytes are
                // the value"; value-returning byte computations (`hostname`:
                // String, `to-json`: Bytes, `echo log; length xs`: Int) keep
                // their proper return value.
                let (bound_ty, rhs_output) =
                    if let CompTy::Fun(..) = self.ctx.unifier.resolve_comp_ty(&inner_ty) {
                        (Ty::Thunk(Box::new(inner_ty)), PipeMode::None)
                    } else {
                        let (ty, _, _) = self.extract_return(&inner_ty);
                        let final_output = self.final_output_of_comp(inner, &inner_ty);
                        (self.observed_value_ty(ty, final_output), final_output)
                    };
                self.ctx
                    .bind_outputs
                    .insert(std::ptr::from_ref::<Comp>(comp) as usize, rhs_output);

                match pattern {
                    IrPattern::Name(name) => {
                        self.ctx
                            .bind_tys
                            .insert(std::ptr::from_ref::<Comp>(comp) as usize, bound_ty.clone());
                        let scheme = generalize(&mut self.ctx.unifier, self.env, &bound_ty);
                        self.env.bind(name.clone(), scheme);
                    }
                    other => {
                        let concrete = self.ctx.unifier.apply_ty(&bound_ty);
                        self.bind_pattern(other, &concrete);
                    }
                }
                self.infer_comp(rest)
            }
            CompKind::App { head, args } => {
                let head_ty = self.infer_comp(head);
                // Surface the common surface error — a literal value
                // (`'foo'`, `42`, ...) used as a command head with args —
                // before falling into the general `Cmd a vs a → b`
                // mismatch path, which prints implementation jargon.
                // We only flag this when there's at least one positional
                // arg; a spread-only call still wants the cascading check.
                let positional = crate::ir::args::positional(args).unwrap_or_default();
                if !positional.is_empty()
                    && let Some(ty) = self.command_non_function_ty(&head_ty)
                {
                    let split_string_suspect = looks_like_nested_quote_mistake(head, &positional);
                    self.ctx.diagnose(TypeErrorKind::CommandNotFunction {
                        ty,
                        split_string_suspect,
                    });
                    // Still type-check the args for cascading errors, then
                    // return a fresh result so the outer pipeline / chain
                    // type-checks against something coherent.
                    for sub in crate::ir::args::iter_subvals(args) {
                        let _ = self.infer_val(sub);
                    }
                    let result = self.ctx.unifier.fresh_comp_ty();
                    self.record_final_output(comp, &result);
                    return result;
                }
                self.apply_args(head_ty, args)
            }
            CompKind::Exec(e) => match &e.head {
                CommandWord::Name(CommandName::Bare(name)) => {
                    self.exec_comp_ty(name, &e.args, false)
                }
                CommandWord::External(CommandName::Bare(name)) => {
                    self.exec_comp_ty(name, &e.args, true)
                }
                CommandWord::Name(CommandName::Path(_) | CommandName::TildePath(_))
                | CommandWord::External(CommandName::Path(_) | CommandName::TildePath(_)) => {
                    self.external_exec_comp_ty(&e.args)
                }
            },
            CompKind::Pipeline { stages, .. } => self.infer_pipeline(stages),
            CompKind::Chain(parts) => self.infer_chain(parts),
            CompKind::Binary(op, lhs, rhs) => CompTy::pure(self.infer_binary(*op, lhs, rhs)),
            CompKind::Not(val) => CompTy::pure(self.infer_not(val)),
            CompKind::Interpolation(parts) => {
                for value in parts {
                    let _ = self.infer_val(value);
                }
                CompTy::pure(Ty::String)
            }
            CompKind::Index { target, keys } => self.infer_index(target, keys),
            CompKind::Seq(comps) => {
                // Run the seq inside a fresh TyEnv frame so alias bindings
                // introduced by statements in this Seq do not leak past the
                // Seq's lexical extent.  The alias-binding logic lives in
                // `infer_seq_with_alias_bindings`; `with_scope` supplies the
                // push/pop.
                self.with_scope(|this| this.infer_seq_with_alias_bindings(comps, Ty::Unit))
            }
            CompKind::LetRec {
                slot: None,
                bindings,
            } => self.infer_letrec(bindings),
            CompKind::LetRec {
                slot: Some(i),
                bindings,
            } => self.infer_letrec_slot(bindings, *i),
            CompKind::If { cond, then, else_ } => {
                let cond_ty = self.infer_val(&cond.item);
                // Narrow pos to the cond's own span (when the parser
                // captured one) before the Bool unify so a non-Bool
                // diagnostic underlines just the cond, not the whole
                // `if … else …` form.
                self.with_span(cond.span, |this| {
                    this.ctx.unify_ty(&cond_ty, &Ty::Bool, Reason::IfCond);
                });
                let then_cty = self.infer_comp(then);
                let else_cty = self.infer_comp(else_);
                self.merge_branches(vec![then_cty, else_cty], &Reason::IfBranches)
            }
            CompKind::Case { scrutinee, table } => self.infer_case(scrutinee, table),
            CompKind::Scope(op) => match op {
                ScopeOp::Within { opts, body } => self.infer_within(opts, body),
                ScopeOp::Grant { caps, body } => self.infer_grant(caps, body),
                ScopeOp::Try { body, handler } => self.infer_try(body, handler),
                ScopeOp::Guard { body, cleanup } => self.infer_guard(body, cleanup),
                ScopeOp::Audit { body } => self.infer_audit(body),
                // Redirect-frame scope: a transparent passthrough for
                // the body's I/O modes — the redirect installs fds
                // for the body's duration but does not change its
                // type signature.  Unlike the other scope ops the
                // body is an `Arc<Comp>` rather than a thunk-shaped
                // `Val`, so we infer it directly.
                ScopeOp::Redirect { body, .. } => self.infer_comp(body),
            },
        };
        self.record_final_output(comp, &cty);
        cty
    }
}
