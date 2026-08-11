//! Type synthesis for the CBPV pair: `infer_val` yields a `Ty`, `infer_comp` a
//! `CompTy`, mutually recursive through thunks.

use super::builtins::{FieldSchema, plugin_entry_field_ty};
use super::env::{InferCtx, TyEnv};
use super::error::{CompDiff, PinFailure, Reason, TypeErrorKind};
use super::generalize::{generalize, instantiate};
use super::scheme::Scheme;
use super::ty::{CompTy, GroundRoute, PayloadRoute, Row, Ty};
use crate::ir::{
    CommandName, CommandWord, Comp, CompKind, IrPattern, ScopeOp, Val, ValListElem, ValMapEntry,
};
use crate::source::Span;
use crate::source::WithSpan;
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

/// One `case` arm as it was written: the handler value itself, whose address
/// keys the result `annotate` reads back, and — for an arm written inline —
/// its body's span, so an arm-local complaint underlines the arm and not the
/// whole `case` form.
struct ArmSyntax<'a> {
    span: Option<crate::source::Span>,
    val: &'a Val,
}

/// Each `case` arm's tag label paired with its [`ArmSyntax`].  An arm's
/// spelling is not its identity: a named handler is as much an arm as an
/// inline thunk, and its route is recorded just the same.  Only the span
/// differs — a named arm has no body to underline, so the caller falls back
/// to the `case` span.  An entry whose key is not a literal tag is no arm at
/// all and is skipped.
fn collect_handler_arms(table: &Val) -> std::collections::HashMap<String, ArmSyntax<'_>> {
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
        out.insert(
            raw_key.to_string(),
            ArmSyntax {
                span: match value {
                    Val::Thunk(inner) => handler_body_span(inner),
                    _ => None,
                },
                val: value,
            },
        );
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

    /// Head `name`'s known payload route: from its handler scheme when one is
    /// in scope, else from a base frame's `Sig`.  A plain native pins to
    /// nothing — only `^name` reaches an arm under it — and an unknown head
    /// or non-`Return` scheme gets a fresh route, leaving the grounding
    /// obligation to whoever settles it.
    fn head_pipe_route(&mut self, name: &str) -> PayloadRoute {
        if let Some(handler) = self.env.lookup_handler(name).cloned() {
            let cty = self.instantiate_comp(&handler.scheme);
            return self.comp_route(&cty);
        }
        if let Some(entry) = self.env.builtins.get(name)
            && entry.fixed_arity().is_none()
            && let super::builtins::BuiltinTypeRule::Sig(sig) = entry.type_rule
        {
            return super::builtins::sig_route(&sig.result, &mut self.ctx.unifier);
        }
        self.ctx.unifier.fresh_route()
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

    /// An external's payload is always captured from its stdout: the one
    /// byte-routed computation, WF-2 by construction.
    fn external_exec_comp_ty(&mut self, args: &crate::ir::Args) -> CompTy {
        self.infer_args(args);
        CompTy::bytes()
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
        }
        // A sequence *is* its tail: a discarded statement ran for its effect
        // and left nothing behind for a later boundary to observe.  Forcing
        // the tail to `Return` here would pin a still-unknown tail out of
        // ever becoming a function.
        last
    }

    /// `a ? b ? c` yields whichever arm succeeds, so every arm must agree on
    /// one payload route and value — exactly the discipline
    /// [`Self::merge_branches`] already applies to `if`'s two arms, here
    /// folded over the whole chain.
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

    /// A `|` is a positional operating-system byte wire: the runtime
    /// connects stdout to stdin and discards every non-final return, so no
    /// adjacency obligation links a stage to its neighbour.  The one static
    /// premise a stage still carries is its own: it must be a
    /// computation ready to run, not a `Fun` still waiting for its argument,
    /// forced under its own [`Reason::PipelineStageShape`] rather than
    /// [`Self::extract_return`]'s generic one, to earn the shape's own hint
    /// text.  The pipeline's own route and value type are then one
    /// projection of the *final* stage's forced shape — never `comp_route`
    /// peering past an arrow into a lambda body.
    fn infer_pipeline(&mut self, comp: &Comp, stages: &[Arc<Comp>]) -> CompTy {
        // The parser unwraps a single-stage pipeline to the bare stage and the
        // elaborator preserves that shape, so a `Pipeline` node always has two.
        debug_assert!(stages.len() >= 2, "Pipeline carries ≥2 stages");

        let mut final_shape = None;
        for stage in stages {
            let cty = self.infer_comp(stage);
            let (value, route) = self.with_span(stage.span, |this| {
                this.force_return_shape(&cty, Reason::PipelineStageShape)
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

    /// Check one `case` arm against its own `arm_cty` — the arms join, they do
    /// not unify — and the scrutinee's resolved per-label payload row
    /// `scrut_payloads`.
    fn check_case_arm(
        &mut self,
        label: &str,
        handler_ty: &Ty,
        arm_cty: &CompTy,
        scrut_payloads: &std::collections::HashMap<String, Ty>,
    ) -> Ty {
        let payload_ty = self.ctx.unifier.fresh_ty();
        let expected = Ty::Thunk(Box::new(CompTy::Fun(
            Box::new(payload_ty.clone()),
            Box::new(arm_cty.clone()),
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

        let arm_syntax = collect_handler_arms(&table.item);

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

        // Each handler at `l` is a thunk of `payload_l → arm_l`, every arm
        // with its own computation type: exactly one arm runs, so the arms
        // join like `if`'s branches rather than unifying.  Each arm's route
        // is recorded for `annotate` — a var now, ground once the join
        // settles it.  The closed scrutinee row is built from the payloads as
        // we go.
        let mut closed_scrut = Row::Empty;
        let mut arms = Vec::with_capacity(handler_labels.len());
        for (label, handler_ty) in handler_labels.iter().rev() {
            let arm = arm_syntax.get(label.as_str());
            let arm_cty = self.ctx.unifier.fresh_comp_ty();
            let closed_payload = self.with_span(arm.and_then(|a| a.span), |this| {
                this.check_case_arm(label, handler_ty, &arm_cty, &scrut_payloads)
            });
            if let Some(arm) = arm {
                let (_, route) = self.extract_return(&arm_cty);
                self.ctx
                    .val_results
                    .insert(std::ptr::from_ref::<Val>(arm.val) as usize, route);
            }
            arms.push(arm_cty);
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

        // No visible arms — the table came from a parameter — constrains
        // nothing here; the case's type is the caller's to pin.
        if arms.is_empty() {
            return self.ctx.unifier.fresh_comp_ty();
        }
        self.merge_branches(arms, &Reason::CaseArms)
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
        self.ctx.solve_at_boundary(self.env);
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
                // A `Fun` RHS is a lambda: evaluating it builds a closure,
                // nothing to capture.  Otherwise the binder consumes the
                // RHS's *payload* — a byte payload through the `Capture`
                // coercion, as the bound `String`; a value payload directly.
                // An open route defaults to `Value`: nothing pinned it to
                // `Bytes`, so there is nothing here to capture.
                let bound_ty = if let CompTy::Fun(..) = self.ctx.unifier.resolve_comp_ty(&inner_ty)
                {
                    Ty::Thunk(Box::new(inner_ty))
                } else {
                    let (ty, route) = self.extract_return(&inner_ty);
                    if matches!(
                        self.ctx.unifier.resolve_route(&route),
                        PayloadRoute::Var(_)
                    ) {
                        self.ctx
                            .unify_route(&route, &PayloadRoute::Value, Reason::RoutePin);
                    }
                    if self.ctx.ground(route) == GroundRoute::Bytes {
                        Ty::String
                    } else {
                        ty
                    }
                };

                if let IrPattern::Name(_) = pattern {
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
            CompKind::Pipeline { stages, .. } => self.infer_pipeline(comp, stages),
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
                    CompTy::Return(sig.route, Box::new(sig.value))
                }
                ScopeOp::Grant { caps, body } => {
                    let sig = self.infer_grant(caps, body);
                    CompTy::Return(sig.route, Box::new(sig.value))
                }
                ScopeOp::Try { body, handler } => {
                    let sig = self.infer_try(body, handler);
                    CompTy::Return(sig.route, Box::new(sig.value))
                }
                ScopeOp::Guard { body, cleanup } => {
                    let sig = self.infer_guard(body, cleanup);
                    CompTy::Return(sig.route, Box::new(sig.value))
                }
                ScopeOp::Audit { body } => {
                    let sig = self.infer_audit(body);
                    CompTy::Return(sig.route, Box::new(sig.value))
                }
                // A redirect installs fds for the body's duration without
                // touching its type.  Its body is an `Arc<Comp>`, not a
                // thunk-shaped `Val` like the other scope ops, so infer it
                // directly.
                ScopeOp::Redirect { body, .. } => self.infer_comp(body),
            },
            // Inserted by `annotate`'s write-back pass, so it is absent from a
            // freshly elaborated tree but present in every tree re-inferred
            // from a live value — a handler arm vetted at install, a bound
            // lambda's body.  Its own route is fixed `Value`: whatever the
            // body's route is, `Capture` is what moves the payload off the
            // byte channel, exactly, as `Bytes`.  Reading those bytes as text
            // is the composed `__decode-captured` step's job, so a capture
            // over an opaque force still says what it has always said —
            // `!{ !$fa }` lets `fa`'s statements be seen.
            CompKind::Capture(body) => {
                let body_ty = self.infer_comp(body);
                let _ = self.extract_return(&body_ty);
                CompTy::pure(Ty::Bytes)
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
