//! Type synthesis for the CBPV pair: `infer_val` yields a `Ty`, `infer_comp` a
//! `CompTy`, mutually recursive through thunks.

use super::builtins::{
    BuiltinDiagnostic, FieldSchema, fail_status_is_zero_literal, plugin_entry_field_ty,
};
use super::env::{InferCtx, TyEnv};
use super::error::{CompDiff, PinFailure, Reason, StdinFeed, TypeErrorKind};
use super::generalize::{generalize, instantiate};
use super::scheme::Scheme;
use super::ty::{CompTy, GroundRoute, PayloadRoute, Row, Ty};
use crate::ir::{
    ArmBody, CaseArm, CommandName, CommandWord, Comp, CompKind, IrPattern, Phrase, Register,
    Toplevel, Val, ValListElem, ValMapEntry,
};
use crate::source::Span;
use crate::source::Spanned;
use crate::source::WithSpan;
use crate::syntax::ast::{BinaryOp, BinaryOpKind, RedirectMode};
use crate::syntax::tag::tag_row_label;
use crate::types::{BuiltinEntry, RefusedArg};
use std::sync::Arc;

/// Which argv boundary `argv_ty` is walking, and so what crosses it.
///
/// The two are ral's own asymmetry: an argv the shell renders itself is total —
/// every value has a text form — while one heading for `execve(2)` is a list of
/// operating-system words, and some shapes are not words.
#[derive(Clone, Copy)]
enum ArgvBoundary<'a> {
    /// A handler arm or a base frame, which refuse nothing.
    InShell,
    /// An external, named as its diagnostics will name it.
    Exec(&'a str),
}

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

/// The redirect binding standard input at a stage's own root, if it has one.
///
/// The root is where a feed answers the stage's reads for its whole run: an
/// [`Exec`](CompKind::Exec) fuses its redirects into the spawn, anything else
/// wears them as a [`CompKind::Redirect`] frame, and the `Bind` arm walks past
/// the binders elaboration hoists out of a redirect's own target
/// (`b < $[locate f]`), whose innermost continuation is the stage as written.
/// A read redirected deeper answers one command's reads and no others, which
/// is that command's business alone, so this walk never sees it.
fn stage_root_stdin_feed(stage: &Comp) -> Option<StdinFeed> {
    let redirects = match &stage.item {
        CompKind::Exec(exec) => &exec.redirects,
        CompKind::Redirect { redirects, .. } => redirects,
        CompKind::Bind { rest, .. } | CompKind::Source { rest, .. } => {
            return stage_root_stdin_feed(rest);
        }
        _ => return None,
    };
    redirects.iter().find_map(|r| match (r.fd, r.mode) {
        (0, RedirectMode::Read) => Some(StdinFeed::File),
        (0, RedirectMode::HereString) => Some(StdinFeed::HereString),
        _ => None,
    })
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

/// Type-check a whole [`Toplevel`]: infer each phrase in order, extending
/// `TyEnv` at each `Define` — a `Source` binds nothing statically — and
/// binding/unbinding an `alias`/`unalias` `Run` phrase's handler scheme for
/// the phrases after it, as `Bind` on `Wildcard` does for a nested discarded
/// statement (§3.5).  Returns each `Define` phrase's generalised per-name
/// schemes, parallel to `top.phrases` and empty for every other phrase —
/// `annotate::annotate_toplevel` writes it straight onto the rebuilt
/// `Phrase::Define`.
pub fn infer_toplevel(
    ctx: &mut InferCtx,
    env: &mut TyEnv,
    top: &Toplevel,
) -> Vec<Vec<(String, Scheme)>> {
    let mut inferencer = Inferencer { ctx, env };
    inferencer.infer_phrases(&top.phrases)
}

/// Every `Name` an `IrPattern` binds, in pattern order — the phrase-level
/// analogue of [`Inferencer::bind_pattern`]'s own walk, kept separate since
/// this one only collects, never binds.
fn collect_pattern_names<'a>(pat: &'a IrPattern, out: &mut Vec<&'a str>) {
    match pat {
        IrPattern::Wildcard => {}
        IrPattern::Name(name) => out.push(name),
        IrPattern::List { elems, rest } => {
            for elem in elems {
                collect_pattern_names(elem, out);
            }
            if let Some(rest_name) = rest {
                out.push(rest_name);
            }
        }
        IrPattern::Map(entries) => {
            for entry in entries {
                collect_pattern_names(&entry.pattern, out);
            }
        }
    }
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

    /// Run `f` with a non-function command head blamed on `why` — the voice of
    /// the form that put the head there — restoring the previous voice on the
    /// way out, as [`WithSpan::with_span`] does for `pos`.  A form that owns
    /// the head owns the whole computation it built around it, so the voice
    /// carries down the spine without anyone having to say what that spine
    /// looks like.
    fn with_command_head_reason<T>(&mut self, why: Reason, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = self.ctx.command_head_reason.replace(why);
        let out = f(self);
        self.ctx.command_head_reason = saved;
        out
    }

    /// Run `f` with no command-head voice: a thunk's body is code of its own,
    /// and a bad head inside it is the author of that body's mistake, whatever
    /// form the thunk happens to sit in.
    fn without_command_head_reason<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = self.ctx.command_head_reason.take();
        let out = f(self);
        self.ctx.command_head_reason = saved;
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

    /// What a binder's pattern reaches from its RHS's inferred type: a `Fun`
    /// RHS is a lambda — evaluating it builds a closure, nothing to capture —
    /// so the whole arrow is thunked; otherwise the binder consumes the
    /// RHS's *payload*, a byte payload through the `Capture` coercion as the
    /// bound `String`, a value payload directly.  An open route defaults to
    /// `Value`: nothing pinned it to `Bytes`, so there is nothing here to
    /// capture.  Shared by `Bind` and `Phrase::Define` (§3.5).
    fn rhs_bound_ty(&mut self, inner_ty: CompTy) -> Ty {
        if let CompTy::Fun(..) = self.ctx.unifier.resolve_comp_ty(&inner_ty) {
            Ty::Thunk(Box::new(inner_ty))
        } else {
            let (ty, route) = self.extract_return(&inner_ty);
            if matches!(self.ctx.unifier.resolve_route(&route), PayloadRoute::Var(_)) {
                self.ctx
                    .unify_route(&route, &PayloadRoute::Value, Reason::RoutePin);
            }
            if self.ctx.ground(route) == GroundRoute::Bytes {
                Ty::String
            } else {
                ty
            }
        }
    }

    /// `cty`'s curried arity: `0` for anything not a `Fun`.
    fn fun_arity(&mut self, cty: &CompTy) -> usize {
        match self.ctx.unifier.resolve_comp_ty(cty) {
            CompTy::Fun(_, body) => 1 + self.fun_arity(&body),
            _ => 0,
        }
    }

    /// Record `rhs`'s curried arity for `annotate`'s η-expansion (S3), keyed
    /// by `rhs`'s own address — absent means "not `Fun`-shaped".
    fn record_arrow_arity(&mut self, rhs: &Arc<Comp>, cty: &CompTy) {
        let arity = self.fun_arity(cty);
        if arity > 0 {
            self.ctx
                .rhs_arrow_arity
                .insert(std::ptr::from_ref::<Comp>(rhs.as_ref()) as usize, arity);
        }
    }

    /// Force `cty` to `Return` shape and read off its payload type and route.
    /// A still-open comp var is unified into that shape rather than rejected
    /// — it may be a free var at an ungeneralized definition site — while a
    /// `Fun` fails under [`Reason::ReturnShape`].
    pub(super) fn extract_return(&mut self, cty: &CompTy) -> (Ty, PayloadRoute) {
        self.force_return_shape(cty, Reason::ReturnShape)
    }

    /// [`Self::extract_return`], reported under a caller-chosen [`Reason`] —
    /// the pipeline stage forcer wants its own hint, not the generic one.
    fn force_return_shape(&mut self, cty: &CompTy, why: Reason) -> (Ty, PayloadRoute) {
        if let CompTy::Return(route, ty) = self.ctx.unifier.resolve_comp_ty(cty) {
            (*ty, route)
        } else {
            let ty = self.ctx.unifier.fresh_ty();
            let route = self.ctx.unifier.fresh_route();
            let expected = CompTy::Return(route, Box::new(ty.clone()));
            self.ctx.unify_comp_ty(cty, &expected, why);
            (ty, route)
        }
    }

    /// A computation demanded ready to run — not a `Fun` still waiting for an
    /// argument — names the verb when it fails as a bare under-applied
    /// builtin, rather than reporting through the unifier as an anonymous
    /// mismatch.  Shared by a discarded value (extended to a non-tail `Seq`
    /// part and the program's own value) and a pipeline stage; `why` is the
    /// fallback [`Reason`] each wants for the ordinary shape mismatch.
    fn force_ready_shape(&mut self, comp: &Comp, cty: &CompTy, why: Reason) -> (Ty, PayloadRoute) {
        if !matches!(self.ctx.unifier.resolve_comp_ty(cty), CompTy::Fun(..)) {
            return self.force_return_shape(cty, why);
        }
        let tail = Self::discard_tail(comp);
        self.with_span(tail.span, |this| match this.discarded_builtin_arity(tail) {
            Some((name, expected, got)) => {
                this.ctx.diagnose(TypeErrorKind::BuiltinArity {
                    name,
                    expected,
                    got,
                });
                (this.ctx.unifier.fresh_ty(), this.ctx.unifier.fresh_route())
            }
            None => this.force_return_shape(cty, why),
        })
    }

    /// [`Self::force_ready_shape`] for a discarded value, whose result no one
    /// reads.
    pub(super) fn force_discarded_shape(&mut self, comp: &Comp, cty: &CompTy) {
        let _ = self.force_ready_shape(comp, cty, Reason::DiscardedValueShape);
    }

    /// The statement whose type `comp`'s own type actually is: a `Bind`'s
    /// type is its `rest`'s and a `Source`'s is its `rest`'s, all the way
    /// down, so `let a = 1; let b = 2; cd`'s discarded value is `cd`'s, not
    /// the outermost node's.
    fn discard_tail(comp: &Comp) -> &Comp {
        match &comp.item {
            CompKind::Bind { rest, .. } | CompKind::Source { rest, .. } => {
                Self::discard_tail(rest)
            }
            _ => comp,
        }
    }

    /// `comp`'s own name and written argument count, when it is an `Exec`
    /// head resolving — by `exec_comp_ty`'s own lookup order — to a value
    /// builtin rather than a user binding.  The one case a discarded `Fun`
    /// should name as that verb's own arity error rather than an anonymous
    /// shape mismatch.
    fn discarded_builtin_arity(&self, comp: &Comp) -> Option<(String, usize, usize)> {
        let CompKind::Exec(exec) = &comp.item else {
            return None;
        };
        let CommandWord::Name(CommandName::Bare(name)) = &exec.head else {
            return None;
        };
        if self.env.lookup_binding(name).is_some() {
            return None;
        }
        let entry: BuiltinEntry = self.env.builtins.value(name)?;
        let got = crate::ir::args::positional(&exec.args).map_or(0, |p| p.len());
        Some((name.clone(), entry.fixed_arity(), got))
    }

    /// A computation's own payload route, peering past `Fun` arrows; an
    /// unresolved comp var yields a fresh route.  Unlike [`Self::extract_return`],
    /// this never forces or reports — it reads whatever shape is already
    /// there, which is the right thing only where the shape was vetted
    /// elsewhere (a looked-up head's own scheme, already installed).
    fn comp_route(&mut self, cty: &CompTy) -> PayloadRoute {
        match self.ctx.unifier.resolve_comp_ty(cty) {
            CompTy::Return(route, _) => route,
            CompTy::Fun(_, body) => self.comp_route(&body),
            CompTy::Var(_) => self.ctx.unifier.fresh_route(),
        }
    }

    /// Merge a conditional's or chain's arms over their payload routes:
    /// exactly one runs, so the arms join rather than unify.  All-or-nothing:
    /// if any arm isn't `Return`, every arm instead unifies strictly via
    /// `unify_comp_ty`.
    fn merge_branches(&mut self, arms: Vec<CompTy>, why: &Reason) -> CompTy {
        if arms.is_empty() {
            return CompTy::pure(self.ctx.unifier.fresh_ty());
        }

        // A still-undecided arm — say, a call to a function under inference,
        // as in a recursive branch — joins with the `Return` arms rather than
        // strict-unifying against them, so force it to `Return` shape first,
        // as `try` does for both its arms.
        if arms
            .iter()
            .any(|cty| matches!(self.ctx.unifier.resolve_comp_ty(cty), CompTy::Return(..)))
        {
            for cty in &arms {
                if matches!(self.ctx.unifier.resolve_comp_ty(cty), CompTy::Var(_)) {
                    let _ = self.extract_return(cty);
                }
            }
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

        let mut per_arm = Vec::with_capacity(arms.len());
        for cty in &arms {
            let CompTy::Return(route, ty) = self.ctx.unifier.resolve_comp_ty(cty) else {
                unreachable!("checked all_return above")
            };
            per_arm.push((route, *ty));
        }
        let (route, observed_acc) = self.ctx.join_arm_results(per_arm, why.clone());

        CompTy::Return(route, Box::new(observed_acc))
    }

    fn autoderef_thunk_return(&mut self, mut cty: CompTy) -> CompTy {
        loop {
            match self.ctx.unifier.resolve_comp_ty(&cty) {
                CompTy::Return(_, ty) => match self.ctx.unifier.resolve_ty(&ty) {
                    Ty::Thunk(inner) => cty = *inner,
                    // A still-free head must become a thunk: the machine's
                    // `apply` rule forces a block-shaped `Value::Thunk` callee
                    // before applying args, so a parameter `$f` of unknown
                    // type has to unfold
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

    /// Refuse a spread that reached an application, blaming the spread itself
    /// rather than the whole call.  Only the first is named: the rest are the
    /// same mistake, and one fix answers them all.
    pub(super) fn refuse_spread(&mut self, args: &crate::ir::Args, head: super::error::SpreadHead) {
        let Some(span) = args
            .iter()
            .find(|e| matches!(e.item, ValListElem::Spread(_)))
            .map(|e| e.span)
        else {
            return;
        };
        self.with_span(span, |this| {
            this.ctx
                .diagnose(TypeErrorKind::SpreadIntoApplication { head });
        });
    }

    pub(super) fn apply_args(&mut self, cty: CompTy, args: &crate::ir::Args) -> CompTy {
        self.apply_args_capped(cty, args, usize::MAX).0
    }

    /// [`Self::apply_args`], applying no more than `cap` positionals: the
    /// surplus is still inferred, for the errors inside it, but unified
    /// against nothing — the same zip an over-applied builtin needs so its
    /// one arity diagnostic is not followed by an anonymous mismatch on the
    /// surplus.  Returns the residual type and each *applied* argument's own
    /// inferred type, the latter for a post-check that must see what was
    /// actually passed (`fail`'s error-record `message` field).
    fn apply_args_capped(
        &mut self,
        mut cty: CompTy,
        args: &crate::ir::Args,
        cap: usize,
    ) -> (CompTy, Vec<Ty>) {
        // A value takes its arguments by application, at an arity its own type
        // declares, so it has no argv and `...` has nothing to spread into.
        // Both callers are value-side, so the refusal needs no test on the head:
        // there is no such thing as an open-argv value for it to discriminate.
        // The subexpressions are still inferred, so errors inside them surface.
        let Some(positional) = crate::ir::args::positional(args) else {
            for sub in crate::ir::args::iter_subvals(args) {
                let _ = self.infer_val(sub);
            }
            self.refuse_spread(args, super::error::SpreadHead::Applied);
            return (self.peel_curry_spine(cty), Vec::new());
        };
        let mut applied = Vec::with_capacity(positional.len().min(cap));
        for (i, arg) in positional.into_iter().enumerate() {
            if i >= cap {
                let _ = self.infer_val(arg);
                continue;
            }
            cty = self.autoderef_thunk_return(cty);
            // Underline the offending argument, not the whole call.  A
            // synthetic entry carries no span, and `with_span` leaves pos alone.
            let (result, arg_ty) = self.with_span(args[i].span, |this| {
                let arg_ty = this.infer_val(arg);
                let result = this.ctx.unifier.fresh_comp_ty();
                let expected = CompTy::Fun(Box::new(arg_ty.clone()), Box::new(result.clone()));
                this.ctx.unify_comp_ty(&cty, &expected, Reason::Argument);
                (result, arg_ty)
            });
            cty = result;
            applied.push(arg_ty);
        }
        (cty, applied)
    }

    /// One application path for every registered builtin, `Scheme` and `Sig`
    /// rules alike: refuse a spread, catch over-application, catch a literal
    /// zero-status `fail`, apply at most `entry.fixed_arity()` arguments, then
    /// run the entry's post-check.  Under-application raises nothing here —
    /// the residual arrow is the type, exactly as an under-applied lambda's
    /// is; a *discarded* one is caught downstream, by
    /// [`Self::force_discarded_shape`].
    ///
    /// The manifest's argv half never reaches here: `exec_comp_ty` looks
    /// `entry` up through [`super::env::TyEnv`]'s value half alone, a base
    /// frame being typed as a handler and reached through
    /// [`Self::apply_alias_arm`] instead — which matters, because a base
    /// frame's argv scheme has curry depth 1 while an argv has no arity at
    /// all ([[invariants/fixed-arity]]).
    pub(super) fn apply_builtin(
        &mut self,
        entry: &BuiltinEntry,
        name: &str,
        args: &crate::ir::Args,
    ) -> CompTy {
        let fixed_arity = entry.fixed_arity();

        // There is no positional reading exactly when the call writes a
        // `...`, and a builtin takes its arguments by application, which has
        // no argv to spread into.
        let Some(positional) = crate::ir::args::positional(args) else {
            self.infer_refused_args(args);
            self.refuse_spread(
                args,
                super::error::SpreadHead::Builtin {
                    name: name.into(),
                    arity: fixed_arity,
                },
            );
            // Nothing was applied and nothing is residual: the refusal is the
            // whole story of this call, so its type is the saturated result.
            return self.saturated_result(entry);
        };

        if positional.len() > fixed_arity {
            self.ctx.diagnose(match entry.diagnostic {
                BuiltinDiagnostic::Decoder => {
                    TypeErrorKind::DecoderTakesNoArgument { name: name.into() }
                }
                _ => TypeErrorKind::BuiltinArity {
                    name: name.into(),
                    expected: fixed_arity,
                    got: positional.len(),
                },
            });
        }

        if entry.diagnostic == BuiltinDiagnostic::FailStatusNonzero
            && fail_status_is_zero_literal(args)
        {
            self.ctx.diagnose(TypeErrorKind::FailStatusZero);
        }

        let scheme = (entry.type_rule)(&mut self.ctx.unifier);
        let head_cty = self.instantiate_comp(&scheme);
        let (result, applied) = self.apply_args_capped(head_cty, args, fixed_arity);

        // `fail`'s error-record shape is unified above like any argument; its
        // `message` field's type is the one part of the shape a row cannot
        // state, so it needs its own pass over what was actually passed.
        if entry.diagnostic == BuiltinDiagnostic::FailStatusNonzero
            && let Some(arg_ty) = applied.first()
        {
            self.check_error_message(arg_ty);
        }

        result
    }

    /// Peel `cty`'s whole curry spine, stopping at the first non-`Fun`: what a
    /// head's type becomes when nothing was applied and nothing is residual —
    /// a refused spread is the whole story of the call, so its type is the
    /// saturated result rather than the still-waiting arrow.
    fn peel_curry_spine(&mut self, mut cty: CompTy) -> CompTy {
        loop {
            let CompTy::Fun(_, body) = self.ctx.unifier.resolve_comp_ty(&cty) else {
                return cty;
            };
            cty = *body;
        }
    }

    /// The `CompTy` a fresh instantiation of `entry`'s scheme names once all
    /// of it is (hypothetically) applied — what a refused spread into a
    /// builtin's type is.
    fn saturated_result(&mut self, entry: &BuiltinEntry) -> CompTy {
        let scheme = (entry.type_rule)(&mut self.ctx.unifier);
        let cty = self.instantiate_comp(&scheme);
        self.peel_curry_spine(cty)
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

    /// Infer the arm for head `name`, pin its payload route to the head's,
    /// and generalise.  Reinterpreting a known head preserves that head's
    /// route and a clash becomes a positioned diagnostic under
    /// [`Reason::HandlerRoutePin`]; an unknown head takes whatever route the
    /// arm defines.
    pub(super) fn handler_comp_scheme(&mut self, name: &str, comp: &Comp) -> Scheme {
        let cty = self.infer_handler_comp(comp);
        if let Err(failure) = self.pin_arm_to_head(name, &cty) {
            let kind = match failure {
                PinFailure::Route(m) => TypeErrorKind::RouteMismatch {
                    expected: m.left,
                    actual: m.right,
                },
                PinFailure::ByteHeadReturnsValue(actual) => TypeErrorKind::CompTyMismatch {
                    expected: CompTy::bytes(),
                    actual: CompTy::Return(PayloadRoute::Bytes, Box::new(actual.clone())),
                    diffs: vec![CompDiff::ReturnType {
                        expected: Ty::Unit,
                        actual,
                    }],
                },
            };
            self.ctx.report(kind, Reason::HandlerRoutePin);
        }
        let thunk_ty = Ty::Thunk(Box::new(cty));
        self.ctx.solve_at_boundary(self.env);
        super::generalize::generalize(&mut self.ctx.unifier, self.env, &thunk_ty)
    }

    /// Head `name`'s known payload route, from the handler scheme in scope for
    /// it — a user arm's, or a base frame's, the two being one thing here.  A
    /// plain native pins to nothing — only `^name` reaches an arm under it —
    /// and an unknown head or non-`Return` scheme gets a fresh route, leaving
    /// the grounding obligation to whoever settles it.
    fn head_pipe_route(&mut self, name: &str) -> PayloadRoute {
        match self.env.lookup_handler(name).cloned() {
            Some(handler) => {
                let cty = self.instantiate_comp(&handler.scheme);
                self.comp_route(&cty)
            }
            None => self.ctx.unifier.fresh_route(),
        }
    }

    /// Peel an alias arm's leading `Fun` arrows: the calling convention forces
    /// the arm on the argv list, so the head's payload route lives on the
    /// body's `Return`, past the parameter arrow.
    fn alias_arm_body(&mut self, cty: &CompTy) -> CompTy {
        match self.ctx.unifier.resolve_comp_ty(cty) {
            CompTy::Fun(_, body) => self.alias_arm_body(&body),
            resolved => resolved,
        }
    }

    /// Unify the arm's payload route against head `name`'s.  A pin that
    /// lands on the byte side lands on [`CompTy::bytes`] — WF-2 admits no
    /// other byte-routed computation — so the arm's value unifies with
    /// `Unit` in the same breath, never left to a sibling grounding.
    ///
    /// The only failures are a route clash and a byte-routed arm that still
    /// returns something, both returned rather than reported so
    /// `alias_arm_scheme` in `typecheck.rs` can refuse the install while
    /// `handler_comp_scheme` merely positions it.
    pub(super) fn pin_arm_to_head(&mut self, name: &str, arm: &CompTy) -> Result<(), PinFailure> {
        let body = self.alias_arm_body(arm);
        let (value, route) = self.extract_return(&body);
        let head = self.head_pipe_route(name);
        self.ctx
            .unifier
            .unify_route(&route, &head)
            .map_err(PinFailure::Route)?;
        if matches!(self.ctx.unifier.resolve_route(&route), PayloadRoute::Bytes)
            && self.ctx.unifier.unify_ty(&value, &Ty::Unit).is_err()
        {
            return Err(PinFailure::ByteHeadReturnsValue(
                self.ctx.unifier.apply_ty(&value),
            ));
        }
        Ok(())
    }

    /// Instantiate `scheme` and strip the outer `Thunk` schemes carry; a
    /// non-thunk body yields a fresh comp var.
    fn instantiate_comp(&mut self, scheme: &Scheme) -> CompTy {
        match instantiate(&mut self.ctx.unifier, scheme) {
            Ty::Thunk(body) => *body,
            _ => self.ctx.unifier.fresh_comp_ty(),
        }
    }

    /// Apply an alias/handler arm — a user's or a base frame's — to a call
    /// site's arguments.  A parameterised arm is `Fun(argv, body)`, and the
    /// argv rule says what that parameter is; a nullary arm discards its
    /// arguments, as the runtime does.
    fn apply_alias_arm(&mut self, scheme: &Scheme, args: &crate::ir::Args) -> CompTy {
        let cty = self.instantiate_comp(scheme);
        let argv = self.argv_ty(args, ArgvBoundary::InShell);
        let CompTy::Fun(param, body) = self.ctx.unifier.resolve_comp_ty(&cty) else {
            return cty;
        };
        self.ctx.unify_ty(&param, &argv, Reason::AliasParam);
        *body
    }

    /// The runtime handler calling convention: an arm is forced on the argv, so
    /// a parameter binds [`Ty::argv`] and the arm keeps its `Fun(argv, body)`
    /// shape for [`Self::apply_alias_arm`] to meet a call site with.
    ///
    /// The parameter is `List String` at the arm, not a variable the call site
    /// pins later, so an arm that reads an element as anything else is refused
    /// where the mistake is written rather than where it is called.
    pub(super) fn infer_alias_arm(&mut self, param: Option<&IrPattern>, body: &Comp) -> CompTy {
        match param {
            Some(param) => {
                let argv_ty = Ty::argv();
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

    /// A command head's type, by the lookup order the runtime uses — binding,
    /// value builtin, handler, external.  A binding hit is final, and a
    /// pristine native reaches here only through the rule table, the bindings
    /// harvest walking user scopes alone.
    ///
    /// The four arms are ral's two worlds in order.  The first two are lambda
    /// calculus: arguments by application, at an arity the head's own type
    /// declares, so `...` has no argv to spread into and is refused.  The last
    /// two take an argv, and `...` is exactly its notation.
    fn exec_comp_ty(&mut self, name: &str, args: &crate::ir::Args, external_only: bool) -> CompTy {
        if !external_only && let Some(scheme) = self.env.lookup_binding(name).cloned() {
            return self.apply_scheme(&scheme, args);
        }

        if !external_only && let Some(entry) = self.env.builtins.value(name) {
            return self.apply_builtin(&entry, name, args);
        }

        if let Some(handler) = self.env.lookup_handler(name).cloned() {
            return self.apply_alias_arm(&handler.scheme, args);
        }

        // Anything left is an external command: prelude functions arrive as an
        // `App` on a bound variable, never as a bare `Exec` head.
        self.external_exec_comp_ty(name, args)
    }

    /// An external takes an argv, and its payload is always captured from its
    /// stdout: the one byte-routed computation, WF-2 by construction.
    ///
    /// Nothing here declares a parameter for that argv to meet, so the rule's
    /// contribution is its walk and its refusals rather than its type.
    fn external_exec_comp_ty(&mut self, shown: &str, args: &crate::ir::Args) -> CompTy {
        let _argv = self.argv_ty(args, ArgvBoundary::Exec(shown));
        CompTy::bytes()
    }

    /// The argv rule: a command's arguments are an argv, and an argv is
    /// [`Ty::argv`] — `List String`.  Every element crosses rendered, through
    /// the total text conversion `str` writes and `Display` performs, so an
    /// element's own type constrains the argv's not at all: this is where
    /// `mycmd hello 1 true` becomes three strings.
    ///
    /// One rule for every argv boundary — a handler arm, a base frame, an
    /// external — so the three differ only in the result they name, and in
    /// whether `boundary` gates what crosses.  Each element is still inferred,
    /// under its own span, for the errors inside it, and a `...` must still
    /// spread a list: what its elements are is free, what it is is not.
    fn argv_ty(&mut self, args: &crate::ir::Args, boundary: ArgvBoundary<'_>) -> Ty {
        for entry in args {
            self.with_span(entry.span, |this| match &entry.item {
                ValListElem::Single(arg) => {
                    let ty = this.infer_val(arg);
                    this.gate_exec_arg(&ty, boundary);
                }
                // A spread contributes as many elements as the list holds, and
                // how many that is only the run knows: an empty one contributes
                // none, and refuses nothing.  So its elements are left to the
                // spawn-time gate, which counts them.
                ValListElem::Spread(arg) => {
                    let spread_ty = this.infer_val(arg);
                    let elem = this.ctx.unifier.fresh_ty();
                    this.ctx
                        .unify_ty(&spread_ty, &Ty::List(Box::new(elem)), Reason::ListSpread);
                }
            });
        }
        Ty::argv()
    }

    /// Refuse an argv element the exec boundary has no argument for, one step
    /// before the spawn that would refuse it — which is what makes the promise
    /// that argument-type errors are reported before execution true of an
    /// external's arguments too.
    ///
    /// A concrete type only.  Where the type is still a variable the shape is
    /// genuinely unknown here, and `runtime::command::vet` keeps the question:
    /// nothing that runs today stops running, and the two answers come from one
    /// declaration ([`RefusedArg`]) rather than from two matches that might
    /// drift.
    fn gate_exec_arg(&mut self, ty: &Ty, boundary: ArgvBoundary<'_>) {
        let ArgvBoundary::Exec(command) = boundary else {
            return;
        };
        // The head decides the verdict; the message shows the whole type.
        let head = self.ctx.unifier.resolve_ty(ty);
        if RefusedArg::of_ty(&head).is_none() {
            return;
        }
        let ty = self.ctx.unifier.apply_ty(&head);
        self.ctx.diagnose(TypeErrorKind::ExecArgNotText {
            command: command.to_string(),
            ty,
        });
    }

    /// Infer every argument for the errors inside it, constraining nothing —
    /// what a refused call still owes its subexpressions.
    pub(super) fn infer_refused_args(&mut self, args: &crate::ir::Args) {
        for sub in crate::ir::args::iter_subvals(args) {
            let _ = self.infer_val(sub);
        }
    }

    /// [`infer_toplevel`]'s walk: one phrase at a time, each under its own
    /// span, threading the extended `TyEnv` from one phrase to the next.
    fn infer_phrases(&mut self, phrases: &[Spanned<Phrase>]) -> Vec<Vec<(String, Scheme)>> {
        let tail_index = phrases.len().saturating_sub(1);
        phrases
            .iter()
            .enumerate()
            .map(|(index, phrase)| {
                self.with_span(phrase.span, |this| {
                    this.infer_phrase(&phrase.item, index == tail_index)
                })
            })
            .collect()
    }

    /// One phrase of §3.5.  `is_tail` marks the toplevel's own last phrase:
    /// every `Run`'s value is held to the discarded shape, tail included —
    /// its bytes are never captured into its own report — but the tail's
    /// arrow arity is additionally read, so S3's η-expansion can rebuild it
    /// if it resolved to `Fun`.
    fn infer_phrase(&mut self, phrase: &Phrase, is_tail: bool) -> Vec<(String, Scheme)> {
        match phrase {
            Phrase::Define { pattern, comp, .. } => {
                let inner_ty = self.infer_comp(comp);
                self.record_arrow_arity(comp, &inner_ty);
                let bound_ty = self.rhs_bound_ty(inner_ty);
                self.ctx.solve_at_boundary(self.env);
                let concrete = self.ctx.unifier.apply_ty(&bound_ty);
                self.bind_pattern(pattern, &concrete, BindMode::Let);

                let mut names = Vec::new();
                collect_pattern_names(pattern, &mut names);
                names
                    .into_iter()
                    .map(|name| {
                        let scheme = self
                            .env
                            .lookup_binding(name)
                            .cloned()
                            .expect("bind_pattern just bound every collected name");
                        (name.to_string(), scheme)
                    })
                    .collect()
            }
            // The path is a computation, inferred for the errors inside it;
            // its own names arrive at run time, so the phrase binds nothing
            // statically here (§3.5).
            Phrase::Source { path } => {
                let _ = self.infer_comp(path);
                Vec::new()
            }
            Phrase::Run(comp) => {
                let mut alias_already_typed = false;
                match alias_statement_shape(comp) {
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
                match unalias_statement_shape(comp) {
                    Ok(Some(name)) => {
                        self.env.unbind_removable_handler(name);
                    }
                    Err(msg) => {
                        self.ctx
                            .diagnose(TypeErrorKind::MalformedUnalias { detail: msg });
                    }
                    Ok(None) => {}
                }
                let cty = if alias_already_typed {
                    super::builtins::pure(Ty::Unit)
                } else {
                    self.infer_comp(comp)
                };
                if is_tail {
                    // The run's value is reported (S3's η-expansion may
                    // rebuild a Fun-typed tail into a thunked λ), but a
                    // byte-routed tail — an external command's own stdout —
                    // is still held to the discarded shape: nothing decodes
                    // the run's own report as text, so its bytes must reach
                    // the visible stream rather than a checker-inserted
                    // `Capture`.
                    self.record_arrow_arity(comp, &cty);
                }
                self.force_discarded_shape(comp, &cty);
                Vec::new()
            }
        }
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
            Val::String(_) => Ty::String,
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
                                // A base frame is a builtin *and* a handler:
                                // name the builtin, which is what the user
                                // wrote and what `explain` documents.
                                None if self.env.builtins.get(name).is_some() => {
                                    self.ctx.diagnose(TypeErrorKind::BuiltinNotFirstClass {
                                        name: name.clone(),
                                    });
                                    self.ctx.unifier.fresh_ty()
                                }
                                None if self.env.lookup_handler(name).is_some() => {
                                    self.ctx.diagnose(TypeErrorKind::HandlerNotFirstClass {
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
            Val::Thunk(comp) => Ty::Thunk(Box::new(self.without_command_head_reason(|this| {
                this.with_scope(|this| this.infer_comp(comp))
            }))),
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

    /// A `|` is a positional operating-system byte wire: the runtime
    /// connects stdout to stdin and discards every non-final return, so no
    /// type of one stage constrains its neighbour's.  Two premises remain,
    /// and each is about a single stage.  Its shape is its own: it must be a
    /// computation ready to run, not a `Fun` still waiting for its argument,
    /// forced under its own [`Reason::PipelineStageShape`] rather than
    /// [`Self::extract_return`]'s generic one, to earn the shape's own hint
    /// text.  Its stdin belongs to the wire: a stage after a `|` may not bind
    /// standard input at its own root, since the feed would answer every read
    /// it makes and leave the producer working for nobody
    /// ([`stage_root_stdin_feed`]).  The pipeline's own route and value type
    /// are then one projection of the *final* stage's forced shape — never
    /// `comp_route` peering past an arrow into a lambda body.
    fn infer_pipeline(&mut self, comp: &Comp, stages: &[Arc<Comp>]) -> CompTy {
        // The parser unwraps a single-stage pipeline to the bare stage and the
        // elaborator preserves that shape, so a `Pipeline` node always has two.
        debug_assert!(stages.len() >= 2, "Pipeline carries ≥2 stages");

        let mut final_shape = None;
        for (position, stage) in stages.iter().enumerate() {
            if position > 0
                && let Some(feed) = stage_root_stdin_feed(stage)
            {
                self.with_span(stage.span, |this| {
                    this.ctx.diagnose(TypeErrorKind::DeadPipeEdge { feed });
                });
            }
            let cty = self.infer_comp(stage);
            let (value, route) = self.with_span(stage.span, |this| {
                this.force_ready_shape(stage, &cty, Reason::PipelineStageShape)
            });
            let key = std::ptr::from_ref::<Comp>(stage.as_ref()) as usize;
            self.ctx.stage_types.insert(key, value.clone());
            final_shape = Some((value, route));
        }
        let (value, route) = final_shape.expect("≥2 stages by invariant above");
        self.ctx
            .pipeline_routes
            .insert(std::ptr::from_ref::<Comp>(comp) as usize, route);
        CompTy::Return(route, Box::new(value))
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

    /// Infer one `case` arm: bind its pattern to a fresh payload type, infer
    /// the body in that scope, then force the payload to agree with what the
    /// scrutinee constructs at this label.  Returns the arm's computation
    /// type and the payload type the closed scrutinee row carries at `label`.
    ///
    /// The agreement is forced here, while pos is still on the arm; the final
    /// row-unify would report it with the caret on the whole `case` form.
    fn infer_case_arm(
        &mut self,
        arm: &CaseArm,
        label: &str,
        scrut_payloads: &std::collections::HashMap<String, Ty>,
    ) -> (CompTy, Ty) {
        let payload_ty = self.ctx.unifier.fresh_ty();
        let arm_cty = self.with_scope(|this| {
            this.bind_pattern(&arm.pattern, &payload_ty, BindMode::Param);
            match &arm.body {
                // The arm the user wrote out is their own code, head and all.
                ArmBody::Inline(body) => this.infer_comp(body),
                // This dispatch is not: the elaborator built it from the atom
                // the arm named, so a head here that is not a function is the
                // arm's fault, wherever in the dispatch it turns up.
                ArmBody::Applied(body) => this
                    .with_command_head_reason(Reason::CaseArmHandler, |this| this.infer_comp(body)),
            }
        });
        // An arm always joins, so force `Return` shape now, before the join
        // reads the arms: an arm still undecided — a call to a function under
        // inference, as in a recursive branch — would otherwise make the whole
        // join strict-unify, equating siblings that only ever needed to
        // subsume.
        let _ = self.extract_return(&arm_cty);
        let closed_payload = self.with_span(arm.body.comp().span, |this| {
            let Some(scrut_payload) = scrut_payloads.get(label) else {
                return payload_ty.clone();
            };
            if this
                .ctx
                .unifier
                .unify_ty(&payload_ty, scrut_payload)
                .is_ok()
            {
                return payload_ty.clone();
            }
            let expected = this.ctx.unifier.apply_ty(scrut_payload);
            let actual = this.ctx.unifier.apply_ty(&payload_ty);
            this.ctx.report(
                TypeErrorKind::TyMismatch { expected, actual },
                Reason::CaseArmPayload,
            );
            this.ctx.unifier.fresh_ty()
        });
        (arm_cty, closed_payload)
    }

    fn infer_case(&mut self, scrutinee: &crate::source::Spanned<Val>, arms: &[CaseArm]) -> CompTy {
        let scrutinee_span = scrutinee.span;
        let scrut_ty = self.with_span(scrutinee_span, |this| this.infer_val(&scrutinee.item));

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

        // Pre-resolved so each arm can unify its payload under its own pos; a
        // residual `Var` here waits for the final row-unify.
        let scrut_resolved_row = self.ctx.unifier.apply_row(&Row::Var(scrut_row_var));
        let scrut_payloads: std::collections::HashMap<String, Ty> =
            collect_extends(&scrut_resolved_row).into_iter().collect();

        // Every arm has its own computation type: exactly one runs, so the
        // arms join like `if`'s branches rather than unifying, and each body's
        // route is recorded for `annotate` by `infer_comp` like any other
        // node's.  Source order throughout, so a program's complaints arrive
        // in the order it was written.
        let mut arm_ctys = Vec::with_capacity(arms.len());
        let mut payloads = Vec::with_capacity(arms.len());
        for arm in arms {
            let label = tag_row_label(&arm.tag.item);
            let (arm_cty, closed_payload) = self.infer_case_arm(arm, &label, &scrut_payloads);
            arm_ctys.push(arm_cty);
            payloads.push((label, closed_payload));
        }
        let closed_scrut = payloads
            .into_iter()
            .rev()
            .fold(Row::Empty, |rest, (l, ty)| {
                Row::Extend(l, Box::new(ty), Box::new(rest))
            });

        // Force the scrutinee row to exactly the arms' label set, restating a
        // row mismatch as exhaustiveness.  The arms are syntax, so this row is
        // always closed and the judgment is always decided: there is no shape
        // of `case` whose alternatives are unknown here.  An *open* scrutinee
        // absorbs a label it has not been seen to construct — that is
        // principal row inference, not a hole in the coverage proof.
        if let Err(kind) = self
            .ctx
            .unifier
            .unify_row(&Row::Var(scrut_row_var), &closed_scrut)
        {
            let translated = match kind {
                TypeErrorKind::RowExtraField { label, .. } => TypeErrorKind::CaseNotExhaustive {
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

        self.merge_branches(arm_ctys, &Reason::CaseArms)
    }

    /// The `Rec` rule of §3.5: bind each name to a self-referential mono
    /// computation type, infer every member in that recursive environment,
    /// unify each against its own type, unbind the self-bindings, and answer
    /// the `index`-th member's type — memoized in `ctx.rec_groups` per
    /// `Arc` identity, so a group is inferred once within a run however many
    /// of its members are projected.
    fn infer_rec(&mut self, group: &Arc<[(String, Arc<Comp>)]>, index: usize) -> CompTy {
        let key = Arc::as_ptr(group).cast::<()>();
        if let Some(betas) = self.ctx.rec_groups.get(&key) {
            return betas[index].clone();
        }

        let betas: Vec<CompTy> = group
            .iter()
            .map(|_| self.ctx.unifier.fresh_comp_ty())
            .collect();

        for ((name, _), beta) in group.iter().zip(betas.iter()) {
            self.env.bind(
                name.clone(),
                Scheme::mono(Ty::Thunk(Box::new(beta.clone()))),
            );
        }
        for ((_, member), beta) in group.iter().zip(betas.iter()) {
            let member_ty = self.infer_comp(member);
            self.ctx.unify_comp_ty(&member_ty, beta, Reason::LetRecSelf);
        }
        for (name, _) in group.iter() {
            self.env.unbind(name);
        }
        self.ctx.rec_groups.insert(key, betas.clone());
        betas[index].clone()
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
            // `Bind` on `Wildcard` is `Seq`'s old per-part duty: the RHS is a
            // discarded statement, so an `alias`/`unalias` shape binds or
            // unbinds its handler scheme over `rest` and anything else is
            // held to `force_discarded_shape` — never generalised, and
            // `rest` inherits whatever scope those bindings opened.
            CompKind::Bind {
                comp: inner,
                pattern,
                rest,
                ..
            } if matches!(pattern.as_ref(), IrPattern::Wildcard) => {
                let mut alias_already_typed = false;
                match alias_statement_shape(inner) {
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
                match unalias_statement_shape(inner) {
                    Ok(Some(name)) => {
                        self.env.unbind_removable_handler(name);
                    }
                    Err(msg) => {
                        self.ctx
                            .diagnose(TypeErrorKind::MalformedUnalias { detail: msg });
                    }
                    Ok(None) => {}
                }
                let inner_ty = if alias_already_typed {
                    super::builtins::pure(Ty::Unit)
                } else {
                    self.infer_comp(inner)
                };
                self.record_arrow_arity(inner, &inner_ty);
                self.force_discarded_shape(inner, &inner_ty);
                self.infer_comp(rest)
            }
            CompKind::Bind {
                comp: inner,
                pattern,
                rest,
                ..
            } => {
                let inner_ty = self.infer_comp(inner);
                self.record_arrow_arity(inner, &inner_ty);
                let bound_ty = self.rhs_bound_ty(inner_ty);

                if let IrPattern::Name(_) = pattern.as_ref() {
                    self.ctx
                        .bind_tys
                        .insert(std::ptr::from_ref::<Comp>(comp) as usize, bound_ty.clone());
                }
                self.ctx.solve_at_boundary(self.env);
                let concrete = self.ctx.unifier.apply_ty(&bound_ty);
                self.bind_pattern(pattern, &concrete, BindMode::Let);
                self.infer_comp(rest)
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
                    let kind = TypeErrorKind::CommandNotFunction {
                        ty,
                        split_string_suspect,
                    };
                    match self.ctx.command_head_reason.clone() {
                        Some(why) => self.ctx.report(kind, why),
                        None => self.ctx.diagnose(kind),
                    }
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
                CommandWord::Name(path @ (CommandName::Path(_) | CommandName::TildePath(_)))
                | CommandWord::External(
                    path @ (CommandName::Path(_) | CommandName::TildePath(_)),
                ) => self.external_exec_comp_ty(&path.written(), &e.args),
            },
            CompKind::Pipeline { stages, .. } => self.infer_pipeline(comp, stages),
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
            CompKind::Rec { group, index } => self.infer_rec(group, *index),
            CompKind::Source { rest, .. } => self.infer_comp(rest),
            CompKind::Observe(reg) => CompTy::pure(match reg {
                Register::Cwd | Register::User | Register::Tilde(_) => Ty::String,
                Register::Args => Ty::List(Box::new(Ty::String)),
                Register::Nproc => Ty::Int,
                // Nothing seeds a static scheme for `$ENV` (`seed_env`), so
                // today it types as an unconstrained fresh variable — the
                // same fallback `Val::Variable`'s lookup miss hits.
                Register::Env => self.ctx.unifier.fresh_ty(),
            }),
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
            CompKind::Case { scrutinee, arms } => self.infer_case(scrutinee, arms),
            CompKind::Within { opts, body } => {
                let sig = self.infer_within(opts, body);
                CompTy::Return(sig.route, Box::new(sig.value))
            }
            CompKind::Grant { caps, body } => {
                let sig = self.infer_grant(caps, body);
                CompTy::Return(sig.route, Box::new(sig.value))
            }
            CompKind::Try { body, handler } => {
                let sig = self.infer_try(body, handler);
                CompTy::Return(sig.route, Box::new(sig.value))
            }
            CompKind::Guard { body, cleanup } => {
                let sig = self.infer_guard(body, cleanup);
                CompTy::Return(sig.route, Box::new(sig.value))
            }
            CompKind::Audit { body } => {
                let sig = self.infer_audit(body);
                CompTy::Return(sig.route, Box::new(sig.value))
            }
            // Installs fds, not its own type; carries the body as an
            // `Arc<Comp>` rather than a thunk-shaped `Val` like the scope
            // forms above, so infer it directly.
            CompKind::Redirect { body, .. } => self.infer_comp(body),
            // Inserted by `annotate`'s write-back pass, so it is absent from a
            // freshly elaborated tree but present in every tree re-inferred
            // from a live value — a handler arm vetted at install, a bound
            // lambda's body.  Its own route is fixed `Value`: whatever the
            // body's route is, `Capture` is what moves the payload off the
            // byte channel, exactly, as `Bytes`.  Reading those bytes as text
            // is the composed `Decode` node's job, so a capture over an opaque
            // force still says what it has always said — `!{ !$fa }` lets
            // `fa`'s statements be seen.
            //
            // The route is deliberately left free: a capture installs a
            // buffer and keeps whatever the body wrote, and where the body's
            // own boundary looks is the body's business — the checker builds
            // `Capture` over a `Value`-routed join arm too (the empty arm of
            // `if c { echo one } else { }`). Only the `Unit` is WF-2's, so
            // only the value unifies.
            CompKind::Capture(body) => {
                let body_ty = self.infer_comp(body);
                let (value, _route) = self.extract_return(&body_ty);
                self.ctx.unify_ty(&value, &Ty::Unit, Reason::CaptureOperand);
                CompTy::pure(Ty::Bytes)
            }
            // The reading half of the same boundary: bytes in, text out. It
            // is syntax rather than a command exactly so that this type is
            // the whole story — nothing a session installs can make the
            // value anything but a `String`. The kernel's `decode` takes a
            // value, so `annotate` reaches it through a bind over `Capture`
            // rather than nesting the two; the operand here is just the
            // bound variable.
            CompKind::Decode(val) => {
                let ty = self.infer_val(val);
                self.ctx.unify_ty(&ty, &Ty::Bytes, Reason::DecodeOperand);
                CompTy::pure(Ty::String)
            }
        };
        if let CompTy::Return(route, _) = self.ctx.unifier.resolve_comp_ty(&cty) {
            self.ctx
                .results
                .insert(std::ptr::from_ref::<Comp>(comp) as usize, route);
        }
        cty
    }
}
