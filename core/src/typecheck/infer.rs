//! Type synthesis for the CBPV pair: `infer_val` yields a `Ty`, `infer_comp` a
//! `CompTy`, mutually recursive through thunks.

use super::builtins::{FieldSchema, plugin_entry_field_ty};
use super::env::{InferCtx, TyEnv};
use super::error::{Reason, TypeErrorKind};
use super::generalize::{generalize, instantiate};
use super::scheme::Scheme;
use super::ty::{ByteMode, CompTy, PipeMode, PipeSpec, Row, Ty};
use crate::ir::{
    CommandName, CommandWord, Comp, CompKind, IrPattern, ScopeOp, Val, ValListElem, ValMapEntry,
};
use crate::source::Span;
use crate::source::WithSpan;
use crate::stream::{done_tag, more_tag};
use crate::syntax::ast::{BinaryOp, BinaryOpKind};
use crate::syntax::tag::{is_tag_label, tag_row_label};
use std::sync::Arc;

/// Labels of a *resolved* row spine in first-appearance order, stopping at the
/// first non-`Extend`.  A repeated label keeps the deepest payload — last-wins,
/// as `Value::map` is at runtime.
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

/// Each `case` arm's tag label paired with its handler body's span, so an
/// arm-local complaint underlines the arm and not the whole `case` form.
/// Anything but a literal tag key over a `Val::Thunk` is skipped, and the
/// caller falls back to the `case` span.
fn collect_handler_spans(table: &Val) -> std::collections::HashMap<String, crate::source::Span> {
    fn handler_body_span(inner: &Comp) -> Option<crate::source::Span> {
        match &inner.item {
            // A `Lam`'s own span is the enclosing statement; the body's is the arm.
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

/// Heuristic: did the lexer close a `"…"` on an unescaped inner quote?  The
/// giveaway is a quoted head whose args mix a string chunk with a hoisted
/// non-string fragment — the interpolation between the quotes, bound out into
/// its own variable.  Bare words are all `Val::String` after [`Val::from_word`],
/// so `'foo' bar baz` falls through to the generic hint.
fn looks_like_nested_quote_mistake(head: &Comp, args: &[&Val]) -> bool {
    let head_from_quoted = matches!(
        head.item,
        CompKind::Return(Val::String(_)) | CompKind::Interpolation(_)
    );
    let any_string_arg = args.iter().any(|a| matches!(a, Val::String(_)));
    let any_non_string_arg = args.iter().any(|a| !matches!(a, Val::String(_)));
    head_from_quoted && any_string_arg && any_non_string_arg
}

/// The elaborator's IR shape for `alias name { body }`.  `Ok(None)` means the
/// head is not `alias` and the caller falls through to a normal exec; `Err`
/// means it is `alias`, malformed.
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

/// Inference state, built directly by the entry points in `typecheck.rs`.
pub(super) struct Inferencer<'a> {
    pub(super) ctx: &'a mut InferCtx,
    pub(super) env: &'a mut TyEnv,
}

impl WithSpan for Inferencer<'_> {
    fn span_slot(&mut self) -> &mut Option<Span> {
        &mut self.ctx.pos
    }
}

/// Whether a pattern binds in let position (names generalize) or as a
/// lambda/handler parameter (names stay monomorphic).
#[derive(Clone, Copy)]
enum BindMode {
    Let,
    Param,
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

    fn bind_pattern(&mut self, pat: &IrPattern, ty: &Ty, mode: BindMode) {
        match pat {
            IrPattern::Wildcard => {}
            IrPattern::Name(name) => {
                let scheme = match mode {
                    BindMode::Let => generalize(&mut self.ctx.unifier, self.env, ty),
                    BindMode::Param => Scheme::mono(ty.clone()),
                };
                self.env.bind(name.clone(), scheme);
            }
            IrPattern::List { elems, rest } => {
                let elem = self.ctx.unifier.fresh_ty();
                self.ctx
                    .unify_ty(ty, &Ty::List(Box::new(elem.clone())), Reason::ListPattern);
                for elem_pat in elems {
                    self.bind_pattern(elem_pat, &elem, mode);
                }
                if let Some(rest_name) = rest {
                    let list_ty = Ty::List(Box::new(elem));
                    let scheme = match mode {
                        BindMode::Let => generalize(&mut self.ctx.unifier, self.env, &list_ty),
                        BindMode::Param => Scheme::mono(list_ty),
                    };
                    self.env.bind(rest_name.clone(), scheme);
                }
            }
            IrPattern::Map(entries) => {
                // Only entries without a default shape the row: a defaulted
                // field may be absent, with the default supplying the binding.
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
                    self.bind_pattern(&entry.pattern, field_ty, mode);
                }
            }
        }
    }

    pub(super) fn extract_return(&mut self, cty: &CompTy) -> (Ty, PipeMode, PipeMode, PipeMode) {
        if let CompTy::Return(spec, ty) = self.ctx.unifier.resolve_comp_ty(cty) {
            (*ty, spec.input, spec.output, spec.result)
        } else {
            let ty = self.ctx.unifier.fresh_ty();
            let input = self.ctx.unifier.fresh_mode();
            let output = self.ctx.unifier.fresh_mode();
            // `cty` may still be a free comp var at an ungeneralized
            // definition site, so `result` mints fresh rather than pinning.
            let result = self.ctx.unifier.fresh_mode();
            let expected = CompTy::Return(
                PipeSpec {
                    input,
                    output,
                    result,
                },
                Box::new(ty.clone()),
            );
            self.ctx.unify_comp_ty(cty, &expected, Reason::ReturnShape);
            (ty, input, output, result)
        }
    }

    /// The value that actually crosses a pipeline's value edge.  The runtime
    /// forces a producer exactly once there — `force_pipe_value` in
    /// `runtime/pipeline.rs`, the twin of this function — so a block producer
    /// contributes its *body's* return type, not the thunk.
    fn deref_forced_producer(&mut self, ty: Ty) -> Ty {
        match self.ctx.unifier.resolve_ty(&ty) {
            Ty::Thunk(inner) => match self.ctx.unifier.resolve_comp_ty(&inner) {
                CompTy::Return(_, inner_ty) => *inner_ty,
                // Forcing a lambda is the identity, so a `Fun`-shaped thunk is
                // itself the produced value: leave it for the consumer.
                _ => ty,
            },
            _ => ty,
        }
    }

    /// One end of a computation's channel spec, peering past `Fun` arrows; an
    /// unresolved comp var yields a fresh mode.
    fn comp_end_mode(&mut self, cty: &CompTy, pick: fn(PipeSpec) -> PipeMode) -> PipeMode {
        match self.ctx.unifier.resolve_comp_ty(cty) {
            CompTy::Return(spec, _) => pick(spec),
            CompTy::Fun(_, body) => self.comp_end_mode(&body, pick),
            CompTy::Var(_) => self.ctx.unifier.fresh_mode(),
        }
    }

    /// A stage's own channel signature, for the annotation pass.  A stage
    /// consumed as a value argument takes its upstream off the value edge, so
    /// its input is `∅`; its output is that of the application result, which
    /// `infer_pipeline` has already rewritten the stage's type to.
    fn stage_own_spec(&mut self, cty: &CompTy, consumed_as_value: bool) -> PipeSpec {
        if consumed_as_value {
            return PipeSpec {
                input: PipeMode::None,
                output: self.comp_output_mode(cty),
                result: PipeMode::None,
            };
        }
        PipeSpec {
            input: self.comp_input_mode(cty),
            output: self.comp_output_mode(cty),
            result: PipeMode::None,
        }
    }

    fn comp_input_mode(&mut self, cty: &CompTy) -> PipeMode {
        self.comp_end_mode(cty, |s| s.input)
    }

    fn comp_output_mode(&mut self, cty: &CompTy) -> PipeMode {
        self.comp_end_mode(cty, |s| s.output)
    }

    /// The mode of two arms only one of which runs, so a clash is not a
    /// contradiction but an unknown: it yields a fresh variable a downstream
    /// stage can pin.  Used for branch arms' input end — the arms read the
    /// same shared stdin, but only one runs, so a mismatch is not fatal.
    pub(super) fn union_mode(&mut self, a: PipeMode, b: PipeMode) -> PipeMode {
        if self.ctx.unifier.unify_mode(&a, &b).is_err() {
            self.ctx.unifier.fresh_mode()
        } else {
            a
        }
    }

    /// Bytes-dominant join: `Bytes` if either end may emit bytes, `None` if
    /// both are silent, else [`Self::union_mode`].  Direction-agnostic — used
    /// for both input and output ends, wherever any one arm's bytes should
    /// dominate the whole.
    pub(super) fn join_byte_mode(&mut self, a: PipeMode, b: PipeMode) -> PipeMode {
        match (
            self.ctx.unifier.resolve_mode(&a),
            self.ctx.unifier.resolve_mode(&b),
        ) {
            (PipeMode::Bytes, _) | (_, PipeMode::Bytes) => PipeMode::Bytes,
            (PipeMode::None, PipeMode::None) => PipeMode::None,
            _ => self.union_mode(a, b),
        }
    }

    /// Merge a conditional's or chain's arms over their result modes: input
    /// unions via [`Self::union_mode`] (only one arm runs), output joins via
    /// bytes-dominant [`Self::join_byte_mode`]. All-or-nothing: if any arm
    /// isn't `Return`, every arm instead unifies strictly via `unify_comp_ty`.
    fn merge_branches(&mut self, arms: Vec<CompTy>, why: &Reason) -> CompTy {
        if arms.is_empty() {
            return CompTy::pure(self.ctx.unifier.fresh_ty());
        }

        let all_return = arms
            .iter()
            .all(|cty| matches!(self.ctx.unifier.resolve_comp_ty(cty), CompTy::Return(..)));

        if !all_return {
            let mut iter = arms.into_iter();
            let mut acc = iter.next().unwrap();
            for branch in iter {
                self.ctx.unify_comp_ty(&acc, &branch, why.clone());
                acc = branch;
            }
            return acc;
        }

        let mut input = None;
        let mut output = None;
        let mut per_arm = Vec::with_capacity(arms.len());
        for cty in &arms {
            let CompTy::Return(spec, ty) = self.ctx.unifier.resolve_comp_ty(cty) else {
                unreachable!("checked all_return above")
            };
            input = Some(match input {
                None => spec.input,
                Some(acc) => self.union_mode(acc, spec.input),
            });
            output = Some(match output {
                None => spec.output,
                Some(acc) => self.join_byte_mode(acc, spec.output),
            });
            per_arm.push((spec, *ty));
        }
        let output = output.unwrap();

        let (result, observed_acc) = self.join_arm_results(&per_arm, why);
        debug_assert!(
            result != PipeMode::Bytes || self.ctx.unifier.resolve_mode(&output) == PipeMode::Bytes,
            "WF-1: result ⊑ output"
        );

        CompTy::Return(
            PipeSpec {
                input: input.unwrap(),
                output,
                result,
            },
            Box::new(observed_acc),
        )
    }

    /// The result-mode join at the heart of every arm merge (`If`/`Chain`
    /// here, `try` in `scope.rs`). No arm ground `Bytes`: still-`Var` arms
    /// pin to `∅` if any arm is, else all unify with each other. Some arm
    /// ground `Bytes`: every arm must land on the byte side, a ground `∅`
    /// arm subsuming there only if its value is `Unit`.
    pub(super) fn join_arm_results(
        &mut self,
        per_arm: &[(PipeSpec, Ty)],
        why: &Reason,
    ) -> (PipeMode, Ty) {
        let any_bytes = per_arm
            .iter()
            .any(|(spec, _)| self.ctx.unifier.resolve_mode(&spec.result) == PipeMode::Bytes);

        if !any_bytes {
            let all_var = per_arm.iter().all(|(spec, _)| {
                matches!(
                    self.ctx.unifier.resolve_mode(&spec.result),
                    PipeMode::Var(_)
                )
            });
            let mut results = per_arm.iter().map(|(spec, _)| spec.result);
            let joined_result = if all_var {
                let first = results.next().expect("a join always has at least one arm");
                for r in results {
                    self.ctx.unify_mode(&first, &r, Reason::ResultPin);
                }
                first
            } else {
                for r in results {
                    self.ctx.unify_mode(&r, &PipeMode::None, Reason::ResultPin);
                }
                PipeMode::None
            };
            let mut iter = per_arm.iter().map(|(_, ty)| ty.clone());
            let first = iter.next().expect("a join always has at least one arm");
            for ty in iter {
                self.ctx.unify_ty(&first, &ty, why.clone());
            }
            return (joined_result, first);
        }

        for (spec, ty) in per_arm {
            match self.ctx.unifier.resolve_mode(&spec.result) {
                PipeMode::Bytes => debug_assert!(
                    matches!(self.ctx.unifier.resolve_ty(ty), Ty::Unit),
                    "WF-2: every arm landing on the byte side returns Unit"
                ),
                PipeMode::Var(_) => {
                    self.ctx
                        .unify_mode(&spec.result, &PipeMode::Bytes, Reason::ResultPin);
                }
                PipeMode::None if matches!(self.ctx.unifier.resolve_ty(ty), Ty::Unit) => {}
                PipeMode::None => {
                    let expected = CompTy::Return(
                        PipeSpec {
                            input: spec.input,
                            output: spec.output,
                            result: PipeMode::Bytes,
                        },
                        Box::new(Ty::Unit),
                    );
                    let actual = CompTy::Return(*spec, Box::new(ty.clone()));
                    self.ctx.unify_comp_ty(&expected, &actual, why.clone());
                }
            }
        }
        (PipeMode::Bytes, Ty::Unit)
    }

    /// Eventual return type, peering past `Fun` arrows as
    /// [`Self::comp_end_mode`] does for modes.  An unresolved `Var` yields a
    /// fresh type, so a caller needing the constraint propagated must unify it
    /// back itself.
    fn comp_return_ty(&mut self, cty: &CompTy) -> Ty {
        match self.ctx.unifier.resolve_comp_ty(cty) {
            CompTy::Return(_, ty) => *ty,
            CompTy::Fun(_, body) => self.comp_return_ty(&body),
            CompTy::Var(_) => self.ctx.unifier.fresh_ty(),
        }
    }

    /// Does this consumer take its upstream as a value argument rather than
    /// over the byte channel?  Function-shaped heads do — a bare lambda, a
    /// block carrying one, a head still free enough to become one — but a
    /// ground `Bytes` input overrides that, and a concrete non-thunk `Return`
    /// (a `from-X` decoder) reads the channel.  Peers past block-literal thunks
    /// as [`Self::autoderef_thunk_return`] does, but plants no constraints.
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
                    // A still-free head must become a thunk: the trampoline's
                    // `apply` forces a `Value::Block` callee before applying
                    // args, so a parameter `$f` of unknown type has to unfold
                    // the same way rather than fail to unify.
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
        // A spread makes the arity dynamic: infer the subexpressions so errors
        // inside them still surface, but constrain no parameter.
        let Some(positional) = crate::ir::args::positional(args) else {
            for sub in crate::ir::args::iter_subvals(args) {
                let _ = self.infer_val(sub);
            }
            return cty;
        };
        for (i, arg) in positional.into_iter().enumerate() {
            cty = self.autoderef_thunk_return(cty);
            // Underline the offending argument, not the whole call.  A
            // synthetic entry carries no span, and `with_span` leaves pos alone.
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

    /// The head's value type when it is concretely not a function — neither a
    /// `Thunk` nor a variable that could still become one.  Lets the `App` rule
    /// name `'foo' bar baz` before the general unifier mismatch fires.
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
        // Only consumers `consumes_value_arg` accepted are routed here, so a
        // clash is a genuine argument-shape error, not a channel adjacency.
        let step_stream = self.piped_ty_is_step_shaped(piped_ty);
        self.ctx
            .unify_comp_ty(&cty, &expected, Reason::PipedValue { step_stream });
        result
    }

    /// Diagnostic only: does the piped value carry a Step tag?  The answer
    /// shapes the hint in [`Self::apply_piped_value`], never the types.
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

    /// The value `from-lines` returns: a recursive Step stream of Strings,
    /// the recursion closing through a comp var, not a `TyVar`.  Shared with
    /// `derive_sig_scheme`'s value form via [`super::builtins::lines_step_ty`].
    pub(super) fn lines_step_ty(&mut self) -> Ty {
        super::builtins::lines_step_ty(&mut self.ctx.unifier)
    }

    /// Check a map literal's entries against a per-key `schema`: every value is
    /// inferred, and a literal key the schema knows also pins its value's type.
    /// Unknown, spread, and dynamic keys stay runtime-dispatched.  Shared by
    /// `within` and `grant` in `typecheck/scope.rs` and by rc plugin entries.
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

    /// An rc `plugins:` list.  Each literal-map entry is checked against the
    /// plugin-entry schema, with no cross-entry unification, so entries of
    /// mixed shape coexist.
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

    /// Instantiate `scheme`, strip its outer `Thunk`, and apply the body to
    /// `args`.  Instantiating here is what keeps quantified variables from
    /// being shared between call sites, so callers hand in a `Scheme` as is.
    pub(super) fn apply_scheme(
        &mut self,
        scheme: &super::scheme::Scheme,
        args: &crate::ir::Args,
    ) -> CompTy {
        let head_cty = self.instantiate_comp(scheme);
        self.apply_args(head_cty, args)
    }

    /// Infer the arm for head `name`, pin its `PipeSpec` to the head's, and
    /// generalise.  Reinterpreting a known head preserves that head's modes and
    /// a clash becomes a positioned [`TypeErrorKind::ModeMismatch`]; an unknown
    /// head takes whatever modes the arm defines.  The arm's value type stays
    /// free.
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

    /// Head `name`'s known `PipeSpec`: from its handler scheme when one is
    /// in scope, else from a base frame's `Sig`.  A plain native pins to
    /// nothing — only `^name` reaches an arm under it — and an unknown head
    /// or non-`Return` scheme gets a fresh `F[μ, ν]`, leaving the
    /// byte-channel discipline to the pipeline edges.
    fn head_pipe_spec(&mut self, name: &str) -> PipeSpec {
        if let Some(handler) = self.env.lookup_handler(name).cloned() {
            let cty = self.instantiate_comp(&handler.scheme);
            if let CompTy::Return(spec, _) = self.alias_arm_body(&cty) {
                return spec;
            }
        }
        if let Some(entry) = self.env.builtins.get(name)
            && entry.fixed_arity().is_none()
            && let super::builtins::BuiltinTypeRule::Sig(sig) = entry.type_rule
        {
            return super::builtins::sig_pipe_spec(&sig.result, &mut self.ctx.unifier);
        }
        self.ctx.unifier.fresh_spec()
    }

    /// Peel an alias arm's leading `Fun` arrows: the calling convention forces
    /// the arm on the argv list, so the head's pipeline modes live on the
    /// body's `Return`, past the parameter arrow.
    fn alias_arm_body(&mut self, cty: &CompTy) -> CompTy {
        match self.ctx.unifier.resolve_comp_ty(cty) {
            CompTy::Fun(_, body) => self.alias_arm_body(&body),
            resolved => resolved,
        }
    }

    /// Unify the arm's `PipeSpec` against head `name`'s, leaving its value type
    /// free.  The only failure is a ground mode clash, returned rather than
    /// reported so `alias_arm_scheme` in `typecheck.rs` can refuse the install
    /// while `handler_comp_scheme` merely positions it.
    pub(super) fn pin_arm_to_head(
        &mut self,
        name: &str,
        arm: &CompTy,
    ) -> Result<(), crate::mode::ModeMismatch> {
        let body = self.alias_arm_body(arm);
        let (_, arm_input, arm_output, _) = self.extract_return(&body);
        let head = self.head_pipe_spec(name);
        self.ctx.unifier.unify_mode(&arm_input, &head.input)?;
        self.ctx.unifier.unify_mode(&arm_output, &head.output)
    }

    /// Instantiate `scheme` and strip the outer `Thunk` schemes carry; a
    /// non-thunk body yields a fresh comp var.
    fn instantiate_comp(&mut self, scheme: &Scheme) -> CompTy {
        match instantiate(&mut self.ctx.unifier, scheme) {
            Ty::Thunk(body) => *body,
            _ => self.ctx.unifier.fresh_comp_ty(),
        }
    }

    /// Apply an alias/handler arm to a call site's arguments.  A parameterised
    /// arm is `Fun(List(elem), body)`, so each argument unifies against `elem`
    /// and each spread against `List(elem)`; without that, an arm whose body
    /// pins `elem` accepts anything and defers the clash to runtime.  A nullary
    /// arm discards its arguments, as the runtime does.
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

    /// The runtime handler calling convention: an arm is forced on the argv
    /// list, so a parameter binds a `Ty::List` and the arm keeps its
    /// `Fun(argv, body)` shape for [`Self::apply_alias_arm`] to unify call-site
    /// arguments against.
    pub(super) fn infer_alias_arm(&mut self, param: Option<&IrPattern>, body: &Comp) -> CompTy {
        match param {
            Some(param) => {
                let elem = self.ctx.unifier.fresh_ty();
                let argv_ty = Ty::List(Box::new(elem));
                let body_cty = self.with_scope(|this| {
                    this.bind_pattern(param, &argv_ty, BindMode::Param);
                    this.infer_comp(body)
                });
                CompTy::Fun(Box::new(argv_ty), Box::new(body_cty))
            }
            None => self.with_scope(|this| this.infer_comp(body)),
        }
    }

    /// The ordinary convention, for a value installed as a lexical binding: a
    /// lambda is `Fun(param, body)` over a fresh value type, a block is its
    /// bare body.  Contrast [`Self::infer_alias_arm`], which forces argv.
    pub(super) fn infer_binding_value(&mut self, param: Option<&IrPattern>, body: &Comp) -> CompTy {
        match param {
            Some(param) => {
                let param_ty = self.ctx.unifier.fresh_ty();
                let body_ty = self.with_scope(|this| {
                    this.bind_pattern(param, &param_ty, BindMode::Param);
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
        // Lookup order — binding, native rule, handler, external — mirroring
        // the runtime's env-first order: a binding hit is final, and a
        // pristine native reaches here only through the rule table, the
        // bindings harvest walking user scopes alone.
        if !external_only && let Some(scheme) = self.env.lookup_binding(name).cloned() {
            return self.apply_scheme(&scheme, args);
        }

        if !external_only && let Some(entry) = self.env.builtins.get(name) {
            use super::builtins::BuiltinTypeRule;
            match entry.type_rule {
                BuiltinTypeRule::Scheme(factory) => {
                    let scheme = factory(&mut self.ctx.unifier);
                    return self.apply_scheme(&scheme, args);
                }
                BuiltinTypeRule::Sig(sig) => return self.apply_builtin_sig(sig, name, args),
            }
        }

        if let Some(handler) = self.env.lookup_handler(name).cloned() {
            return self.apply_alias_arm(&handler.scheme, args);
        }

        // Anything left is an external command: prelude functions arrive as an
        // `App` on a bound variable, never as a bare `Exec` head.
        self.external_exec_comp_ty(args)
    }

    // WF-1 (result ⊑ output) holds by inspection: both are the literal `Bytes` below.
    fn external_exec_comp_ty(&mut self, args: &crate::ir::Args) -> CompTy {
        self.infer_args(args);
        let input = self.ctx.unifier.fresh_mode();
        CompTy::Return(
            PipeSpec {
                input,
                output: PipeMode::Bytes,
                result: PipeMode::Bytes,
            },
            Box::new(Ty::Unit),
        )
    }

    pub(super) fn infer_args(&mut self, args: &crate::ir::Args) {
        for sub in crate::ir::args::iter_subvals(args) {
            let _ = self.infer_val(sub);
        }
    }

    /// Walk a `Seq`, binding each `alias` definition into the current `TyEnv`
    /// scope as it is met so later statements resolve against it.  The caller's
    /// `with_scope` frame is what stops those bindings outliving the `Seq`.
    ///
    /// An alias whose thunk is not a literal lambda is still bound, typed as a
    /// nullary arm, so `g x` is a static arity mismatch rather than a silently
    /// discarded `x`.  Runtime install refuses a bare-block alias outright; the
    /// static layer stays lenient, a thunk's runtime value being unknown here.
    pub(super) fn infer_seq_with_alias_bindings(
        &mut self,
        parts: &[Arc<Comp>],
        empty: Ty,
    ) -> CompTy {
        let mut last = CompTy::pure(empty);
        let mut emits_bytes = false;
        let mut reads_bytes = false;
        for part in parts {
            let mut alias_already_typed = false;
            match alias_statement_shape(part) {
                Ok(Some((name, thunk))) => {
                    let scheme = self.handler_comp_scheme(name, thunk);
                    self.env.bind_handler(name.to_string(), scheme, true);
                    alias_already_typed = true;
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
            // `handler_comp_scheme` above is the sole authority for the arm's
            // type and has already spoken.  Falling through would re-dispatch
            // the same `Exec("alias", …)` through the `ALIAS` builtin sig and
            // duplicate every diagnostic inside the thunk, so synthesize that
            // sig's fixed pure-`Unit` result instead.
            last = if alias_already_typed {
                super::builtins::pure(Ty::Unit)
            } else {
                self.infer_comp(part)
            };
            let out = self.comp_output_mode(&last);
            emits_bytes |= self.ctx.unifier.resolve_mode(&out) == PipeMode::Bytes;
            let inp = self.comp_input_mode(&last);
            reads_bytes |= self.ctx.unifier.resolve_mode(&inp) == PipeMode::Bytes;
        }
        self.lift_modes(last, reads_bytes, emits_bytes)
    }

    /// A sequence's two channels are everything its statements read and write,
    /// so each mode joins over the whole run, not just the last statement — a
    /// body that `echo`es per line is byte-output, and one that reads stdin
    /// before its final statement is byte-input.  Only those modes lift, and a
    /// `Fun`-tailed sequence yields a function, not a stage, so it keeps its
    /// shape.
    fn lift_modes(&mut self, last: CompTy, reads_bytes: bool, emits_bytes: bool) -> CompTy {
        if !(reads_bytes || emits_bytes)
            || matches!(self.ctx.unifier.resolve_comp_ty(&last), CompTy::Fun(..))
        {
            return last;
        }
        let (ret, input, output, result) = self.extract_return(&last);
        CompTy::Return(
            PipeSpec {
                input: if reads_bytes { PipeMode::Bytes } else { input },
                output: if emits_bytes { PipeMode::Bytes } else { output },
                result,
            },
            Box::new(ret),
        )
    }

    /// `a ? b ? c` yields whichever arm succeeds, so every arm must agree on
    /// one result type — exactly the discipline [`Self::merge_branches`]
    /// already applies to `if`'s two arms, here folded over the whole chain.
    /// The output mode is a bytes-dominant join — `tmux a ? tmux b` emits
    /// bytes either way.
    fn infer_chain(&mut self, parts: &[Arc<Comp>]) -> CompTy {
        let arms: Vec<CompTy> = parts.iter().map(|part| self.infer_comp(part)).collect();
        self.merge_branches(arms, &Reason::ChainBranches)
    }

    fn infer_map_val(&mut self, entries: &[ValMapEntry]) -> Ty {
        let all_literal_keys = entries.iter().all(|entry| match entry {
            ValMapEntry::Entry(Val::String(_), _) | ValMapEntry::Spread(_) => true,
            ValMapEntry::Entry(_, _) => false,
        });

        if all_literal_keys && !entries.is_empty() {
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

            // Last-wins on a duplicate key, as `Value::map` is at runtime;
            // first-appearance order only fixes the row spine's shape.
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
            // A dynamic-key map is `Map<elem>` with one `elem` shared by every
            // value and spread: without that, `[$k: 1, $j: "hi"]` checks as
            // `Map<α>` and lets the consumer pick `Map<Int>` over a String.
            // Keys must be `String` — the runtime rejects others at status 1,
            // so `[2: foo]` is lifted to a type error here.
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
                // Construction is open: `` `ok 5 `` gets a fresh row tail.  The
                // label keeps its backtick so unification reads it as a tag —
                // the tag and bare alphabets do not unify.
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
        // The parser unwraps a single-stage pipeline to the bare stage and the
        // elaborator preserves that shape, so a `Pipeline` node always has two.
        debug_assert!(stages.len() >= 2, "Pipeline carries ≥2 stages");

        let mut stage_tys: Vec<CompTy> =
            stages.iter().map(|stage| self.infer_comp(stage)).collect();
        // A stage consumed as a value argument is the data-last fold's
        // function: its upstream arrives as the final argument, so its input
        // channel is `∅` while its output is whatever its body emits.  The
        // rewrite below replaces such a stage's type with its application body,
        // so record the fact here, while the original shape is still visible.
        let mut consumed_as_value = vec![false; stage_tys.len()];
        for i in 0..stage_tys.len() - 1 {
            // Underline the consumer that refuses the upstream; otherwise the
            // caret follows whatever `ctx.pos` the last-inferred stage left, so
            // a clash on an early edge would point at the final stage.
            let edge_span = stages[i + 1].span.or(stages[i].span);
            let out = self.comp_output_mode(&stage_tys[i]);
            let out_resolved = self.ctx.unifier.resolve_mode(&out);

            // A value producer feeding a value-arg consumer is the data-last
            // application `x | f` = `f !{x}`.  Feeding anything else — a
            // concrete `Return` such as a `from-X` decoder — it is a plain
            // channel edge, so `∅`-into-`Bytes` is rejected: values do not
            // silently cross a byte edge, they must be encoded and decoded.
            //
            // An unresolved output is the diverging producer (`{ fail … }`,
            // whose modes are fresh and quantified).  `InferCtx::ground` will
            // settle it to `∅`, so count it a value edge already; otherwise the
            // producer's *thunk* type meets the consumer's parameter and the
            // runtime hands over the unforced block instead of running it.
            let out_is_value_edge = matches!(out_resolved, PipeMode::None | PipeMode::Var(_));
            if out_is_value_edge && self.consumes_value_arg(&stage_tys[i + 1]) {
                let (piped_ty, _, _, _) = self.extract_return(&stage_tys[i]);
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

        // Input from the first stage, output and return type from the last.  A
        // `Fun` tail is the byte-pipe-into-value-arg case (`cat foo | length`),
        // not modelled structurally, so drill past the arrows.  A `Var` tail is
        // unified back against the synthesized `Return`, or the pipeline's own
        // consumers would see an unrelated fresh variable.
        let input = self.comp_input_mode(&stage_tys[0]);
        let last_consumed = consumed_as_value[stage_tys.len() - 1];
        let last = stage_tys
            .last()
            .expect("≥2 stages by invariant above")
            .clone();
        let output = self.stage_own_spec(&last, last_consumed).output;
        let ret_ty = self.comp_return_ty(&last);
        if matches!(self.ctx.unifier.resolve_comp_ty(&last), CompTy::Var(_)) {
            let bound = CompTy::Return(
                PipeSpec {
                    input: self.comp_input_mode(&last),
                    output,
                    result: self.ctx.unifier.fresh_mode(),
                },
                Box::new(ret_ty.clone()),
            );
            self.ctx.unify_comp_ty(&last, &bound, Reason::ReturnShape);
        }

        // For the annotation pass, keyed by node address; still-unresolved
        // modes and vars settle once the whole walk's constraints are in.
        for (i, (stage, ty)) in stages.iter().zip(&stage_tys).enumerate() {
            let spec = self.stage_own_spec(ty, consumed_as_value[i]);
            let value_ty = self.comp_return_ty(ty);
            let key = std::ptr::from_ref::<Comp>(stage.as_ref()) as usize;
            self.ctx.stage_specs.insert(key, spec);
            self.ctx.stage_types.insert(key, value_ty);
        }

        // Byte-tailed (matching `PipelineCollector::finish`): value is `Unit`,
        // result is `Bytes`. Value-tailed keeps the last stage's own `result`.
        let byte_tailed =
            !last_consumed && self.ctx.unifier.resolve_mode(&output) == PipeMode::Bytes;
        let pipeline_ret_ty = if byte_tailed { Ty::Unit } else { ret_ty };
        let pipeline_result = if byte_tailed {
            PipeMode::Bytes
        } else {
            self.comp_end_mode(&last, |s| s.result)
        };
        debug_assert!(
            self.ctx.unifier.resolve_mode(&pipeline_result) != PipeMode::Bytes
                || matches!(self.ctx.unifier.resolve_ty(&pipeline_ret_ty), Ty::Unit),
            "WF-2: a Bytes-result pipeline returns Unit"
        );
        debug_assert!(
            self.ctx.unifier.resolve_mode(&pipeline_result) != PipeMode::Bytes
                || self.ctx.unifier.resolve_mode(&output) == PipeMode::Bytes,
            "WF-1: result ⊑ output"
        );

        CompTy::Return(
            PipeSpec {
                input,
                output,
                result: pipeline_result,
            },
            Box::new(pipeline_ret_ty),
        )
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

    /// One step of an indexing chain, run under the pos `infer_index` narrowed
    /// to this key, so a failure underlines the step and not the whole chain.
    fn infer_index_step(&mut self, current_ty: &Ty, key: &Val) -> Ty {
        let resolved = self.ctx.unifier.apply_ty(current_ty);
        match resolved {
            Ty::List(elem) => {
                let key_ty = self.infer_val(key);
                self.ctx.unify_ty(&key_ty, &Ty::Int, Reason::ListIndexKey);
                *elem
            }
            Ty::Map(elem) => {
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
                // `Val::from_word` leaves a bare non-numeric word a `String`,
                // and only a `String` key reads a record field; a bare number
                // falls to the dynamic arm, no record having an Int field name.
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
                    // A raw unify error on a concretely non-record target reads
                    // `Int vs [b: α, ...ρ]` — accurate but hostile.  Say it in
                    // a sentence instead.
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
                    // Catching this here is what makes `let x = 42; $x[$k]` a
                    // type error rather than a deferred runtime failure.  A
                    // free target is pinned by the key's type — `Int` ⇒ `List`,
                    // `String` ⇒ `Map` — and otherwise left for whatever pins
                    // it later, which re-enters through the arms above.
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
        // Force agreement while pos is still on the arm; the final row-unify
        // would report it with the caret on the whole `case` form.
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
        // CBPV: `case` eliminates a sum with a record of continuations, so both
        // operands sit in value position.
        let scrutinee_span = scrutinee.span;
        let table_span = table.span;
        let scrut_ty = self.with_span(scrutinee_span, |this| this.infer_val(&scrutinee.item));
        let table_ty = self.with_span(table_span, |this| this.infer_val(&table.item));
        let result_cty = self.ctx.unifier.fresh_comp_ty();

        let handler_spans = collect_handler_spans(&table.item);

        // A scrutinee that is concretely not a variant gets a sentence: the raw
        // row mismatch prints `[...ρ]`, which a beginner cannot read.
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

        // A record literal closes to `Empty`, so this is a clean label list
        // under normal use.
        let handler_resolved = self.ctx.unifier.apply_row(&Row::Var(handler_row_var));
        let handler_labels = collect_extends(&handler_resolved);

        // Pre-resolved so each arm can unify its payload under its own pos; a
        // residual `Var` here came from the handlers and waits for the final
        // row-unify.
        let scrut_resolved_row = self.ctx.unifier.apply_row(&Row::Var(scrut_row_var));
        let scrut_payloads: std::collections::HashMap<String, Ty> =
            collect_extends(&scrut_resolved_row).into_iter().collect();

        // Each handler at `l` is a thunk of `payload_l → result_cty`; the
        // closed scrutinee row is built from those payloads as we go.
        let mut closed_scrut = Row::Empty;
        for (label, handler_ty) in handler_labels.iter().rev() {
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

        // Force the scrutinee row to exactly the handler label set, restating a
        // row mismatch as exhaustiveness.  A handler row still a bare variable
        // — the table came from a parameter, not a literal — is *unknown*, not
        // empty: closing it would call every scrutinee tag uncovered.
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

    /// Bind each name to a self-referential mono thunk, infer every RHS in that
    /// recursive environment, and unify each against its own thunk type.  The
    /// mono self-bindings are left installed: `infer_letrec` drops and
    /// generalises them, `infer_letrec_slot` discards the whole scope.
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

    /// `LetRec { slot: Some(i) }` yields binding `i`'s lambda, inferring the
    /// whole group in a throwaway scope so its errors surface and the
    /// self-bindings do not leak.  `eval_letrec` synthesises these nodes, so
    /// the path runs only when such IR is re-checked.
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
        // Drop the mono self-bindings before generalising: left in env, their
        // free vars would count as residuals in `env_free_vars` and
        // `generalize` would refuse to quantify them, silently un-poly'ing
        // every recursive scheme.  Rebound below with the polymorphic ones.
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

    pub(super) fn infer_comp(&mut self, comp: &Comp) -> CompTy {
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
                // A `Fun` RHS is a lambda: evaluating it builds a closure and
                // emits nothing, so its output is `∅`.
                // Its bytes *out* may be captured into the bound value, but its
                // bytes *in* are a demand on the one stdin the binder shares
                // with `rest`, so they lift out of the capture.
                let (bound_ty, rhs_reads) =
                    if let CompTy::Fun(..) = self.ctx.unifier.resolve_comp_ty(&inner_ty) {
                        (Ty::Thunk(Box::new(inner_ty)), false)
                    } else {
                        let (ty, input, _, result) = self.extract_return(&inner_ty);
                        let reads = self.ctx.unifier.resolve_mode(&input) == PipeMode::Bytes;
                        if matches!(result, PipeMode::Var(_)) {
                            self.ctx
                                .unify_mode(&result, &PipeMode::None, Reason::ResultPin);
                        }
                        let observed = if self.ctx.ground(result) == ByteMode::Bytes {
                            Ty::String
                        } else {
                            ty
                        };
                        (observed, reads)
                    };

                if let IrPattern::Name(_) = pattern {
                    self.ctx
                        .bind_tys
                        .insert(std::ptr::from_ref::<Comp>(comp) as usize, bound_ty.clone());
                }
                let concrete = self.ctx.unifier.apply_ty(&bound_ty);
                self.bind_pattern(pattern, &concrete, BindMode::Let);
                let rest_ty = self.infer_comp(rest);
                self.lift_modes(rest_ty, rhs_reads, false)
            }
            CompKind::App { head, args } => {
                let head_ty = self.infer_comp(head);
                // Name a literal used as a command head before the general
                // `Cmd a vs a → b` mismatch says it in jargon.  Needs a
                // positional arg; a spread-only call wants the cascading check.
                let positional = crate::ir::args::positional(args).unwrap_or_default();
                if !positional.is_empty()
                    && let Some(ty) = self.command_non_function_ty(&head_ty)
                {
                    let split_string_suspect = looks_like_nested_quote_mistake(head, &positional);
                    self.ctx.diagnose(TypeErrorKind::CommandNotFunction {
                        ty,
                        split_string_suspect,
                    });
                    // Check the args anyway, then hand the enclosing pipeline
                    // or chain a coherent fresh result.
                    for sub in crate::ir::args::iter_subvals(args) {
                        let _ = self.infer_val(sub);
                    }
                    return self.ctx.unifier.fresh_comp_ty();
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
            // The operand's own type, unconstrained — `Arith` below leaves
            // numeric-ness to the evaluator too, so `-` agrees with `-`.
            CompKind::Negate(val) => CompTy::pure(self.infer_val(val)),
            CompKind::Not(val) => CompTy::pure(self.infer_not(val)),
            CompKind::Interpolation(parts) => {
                for value in parts {
                    let _ = self.infer_val(value);
                }
                CompTy::pure(Ty::String)
            }
            CompKind::Index { target, keys } => self.infer_index(target, keys),
            CompKind::Seq(comps) => {
                // The frame `infer_seq_with_alias_bindings` requires, so its
                // alias bindings die with the `Seq`.
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
                // Underline just the cond, not the whole `if … else …` form.
                self.with_span(cond.span, |this| {
                    this.ctx.unify_ty(&cond_ty, &Ty::Bool, Reason::IfCond);
                });
                let then_cty = self.infer_comp(then);
                let else_cty = self.infer_comp(else_);
                self.merge_branches(vec![then_cty, else_cty], &Reason::IfBranches)
            }
            CompKind::Case { scrutinee, table } => self.infer_case(scrutinee, table),
            CompKind::Scope(op) => match op {
                ScopeOp::Within { opts, body } => {
                    let sig = self.infer_within(opts, body);
                    self.seal(sig)
                }
                ScopeOp::Grant { caps, body } => {
                    let sig = self.infer_grant(caps, body);
                    self.seal(sig)
                }
                ScopeOp::Try { body, handler } => {
                    let sig = self.infer_try(body, handler);
                    self.seal(sig)
                }
                ScopeOp::Guard { body, cleanup } => {
                    let sig = self.infer_guard(body, cleanup);
                    self.seal(sig)
                }
                ScopeOp::Audit { body } => {
                    let sig = self.infer_audit(body);
                    self.seal(sig)
                }
                // A redirect installs fds for the body's duration without
                // touching its type.  Its body is an `Arc<Comp>`, not a
                // thunk-shaped `Val` like the other scope ops, so infer it
                // directly.
                ScopeOp::Redirect { body, .. } => self.infer_comp(body),
            },
            // Checker-inserted only, by `annotate`'s later write-back pass —
            // never present in a tree `infer_comp` walks.
            CompKind::Capture(_) => unreachable!("Capture is annotate-only IR"),
        };
        if let CompTy::Return(spec, _) = self.ctx.unifier.resolve_comp_ty(&cty) {
            self.ctx
                .results
                .insert(std::ptr::from_ref::<Comp>(comp) as usize, spec.result);
        }
        cty
    }
}
