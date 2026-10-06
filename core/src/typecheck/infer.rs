//! Type synthesis for the CBPV pair: `infer_val` yields a `Ty`, `infer_comp` a
//! `CompTy`, mutually recursive through thunks.

use super::builtins::{BuiltinDiagnostic, fail_status_is_zero_literal};
use super::env::{BoundaryValue, HandlerBinding, InferCtx, TyEnv, comp_key, val_key};
use super::error::{Reason, Standing, StdinFeed, TypeError, TypeErrorKind, UnitCall};
use super::generalize::{FreeVars, generalize};
use super::grade::JoinArm;
use super::index::{Idx, Lbl, Settled};
use super::kind::Kind;
use super::scheme::Scheme;
use super::ty::{CompTy, Grade, Label, Producer, Row, Ty};
use super::unify::WeakSource;
use crate::ir::{
    Assembly, CaseArm, CommandName, CommandWord, Comp, CompKind, DefineSchemes, Exec, GroupNode,
    IrPattern, Name, Phrase, Toplevel, Val, ValListElem, ValMapEntry, ValRecordEntry, is_gensym,
};
use crate::source::Span;
use crate::source::Spanned;
use crate::source::WithSpan;
use crate::syntax::ast::{ArithOp, BinaryOp, BinaryOpKind, ScopeAst, StdinSource};
use crate::types::{BuiltinEntry, LANGUAGE_CONSTANTS, RefusedArg};
use std::sync::Arc;

/// What a bare command head resolves to, by the lookup order the checker and
/// the runtime share: binding, value builtin, handler, external.
enum HeadClass {
    /// A lexical or session binding, called at its scheme.
    Binding(Arc<Scheme>),
    /// A value row of the builtin table, applied at its scheme.
    Value(BuiltinEntry),
    /// A handler in scope — a user arm, else a base frame such as `echo` —
    /// standing in for the head it names.
    Arm(HandlerBinding),
    /// Anything else, and always `^name`, `./p` and `~/p`.
    External,
}

/// `head`'s stand-in type: an argv in, a command out.
fn command_head() -> CompTy {
    CompTy::arrows([Ty::argv()], CompTy::command())
}

/// Which argv boundary `argv_ty` is walking, and so what crosses it.
///
/// The two are ral's own asymmetry: an argv the shell renders itself is total
/// but for `()` — every other value has a text form — while one heading for
/// `execve(2)` is a list of operating-system words, and some shapes are not
/// words.
#[derive(Clone, Copy)]
enum ArgvBoundary<'a> {
    /// A handler arm or a base frame, named as its diagnostics will name it;
    /// they refuse nothing but `()`.
    InShell(&'a str),
    /// An external, named as its diagnostics will name it.
    Exec(&'a str),
}

/// Which tags the arms and the value disagree about: a tag the value produces
/// with no arm to take it, and — only where the value's row is closed, an open
/// tail being free to absorb one — an arm for a tag it never produces.
///
/// `None` when the two sets agree, which leaves the row error to speak for
/// itself: the rows then differ in a payload rather than in coverage.
fn coverage_verdict(scrutinee: &Row, arms: &[Label]) -> Option<TypeErrorKind> {
    let produced: Vec<Label> = collect_extends(scrutinee)
        .into_iter()
        .map(|(l, _)| l)
        .collect();
    let missing: Vec<String> = produced
        .iter()
        .filter(|l| !arms.contains(l))
        .map(Label::to_string)
        .collect();
    let extra: Vec<String> = if row_is_open(scrutinee) {
        Vec::new()
    } else {
        arms.iter()
            .filter(|l| !produced.contains(l))
            .map(Label::to_string)
            .collect()
    };
    (!missing.is_empty() || !extra.is_empty())
        .then_some(TypeErrorKind::CaseNotExhaustive { missing, extra })
}

/// True while the row still ends in a tail variable, so a label it does not
/// name may yet turn out to be there.
fn row_is_open(row: &Row) -> bool {
    let mut cur = row;
    loop {
        match cur {
            Row::Extend(_, _, rest) => cur = rest,
            Row::Var(_) => return true,
            Row::Empty => return false,
        }
    }
}

/// Labels of a *resolved* row spine in first-appearance order, stopping at the
/// first non-`Extend`.  A repeated label keeps the head payload, as selection
/// and `unify_row` both do.
fn collect_extends(row: &Row) -> Vec<(Label, Ty)> {
    let mut out: Vec<(Label, Ty)> = Vec::new();
    let mut cur = row;
    loop {
        match cur {
            Row::Extend(l, ty, rest) => {
                if !out.iter().any(|(k, _)| k == l) {
                    out.push((l.clone(), (**ty).clone()));
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

/// A scalar literal as written, the target [`TypeErrorKind::IndexOnLiteral`]
/// refuses; `None` for anything an index may read.
fn spell_literal(v: &Val) -> Option<String> {
    match v {
        Val::String(s) => Some(format!("'{s}'")),
        Val::Int(n) => Some(n.to_string()),
        Val::Float(f) => Some(f.to_string()),
        Val::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// An index key as written between the brackets.
fn spell_key(v: &Val) -> String {
    match v {
        Val::String(s) => s.to_string(),
        Val::Int(n) => n.to_string(),
        Val::Variable(x) if !is_gensym(x) => format!("${x}"),
        _ => "…".into(),
    }
}

/// The redirect binding standard input at a stage's own root, if it has one.
///
/// The root is where a feed answers the stage's reads for its whole run: an
/// [`Exec`](CompKind::Exec) carries its redirects, anything else
/// wears them as a [`CompKind::Redirect`] frame, and the `Bind` arm walks past
/// the binders elaboration hoists out of a redirect's own target
/// (`b < $[locate f]`), whose innermost continuation is the stage as written.
/// A read redirected deeper answers one command's reads and no others, which
/// is that command's business alone, so this walk never sees it.
fn stage_root_stdin_feed(stage: &Comp) -> Option<StdinFeed> {
    let redirects = match &stage.item {
        CompKind::Exec(exec) => &exec.redirects,
        CompKind::Redirect { redirects, .. } => redirects,
        CompKind::Bind { rest, .. } => {
            return stage_root_stdin_feed(rest);
        }
        _ => return None,
    };
    match redirects.stdin {
        Some(StdinSource::File(_)) => Some(StdinFeed::File),
        Some(StdinSource::Here(_)) => Some(StdinFeed::HereString),
        None => None,
    }
}

/// A call of a bare name, as `exec_comp_ty` dispatches on it.
fn bare_call(comp: &Comp) -> Option<(&str, &Exec)> {
    let CompKind::Exec(exec) = &comp.item else {
        return None;
    };
    let CommandWord::Name(CommandName::Bare(head)) = &exec.head else {
        return None;
    };
    Some((head.as_ref(), exec))
}

/// The elaborator's IR shape for `alias name { body }` and `unalias name`,
/// which bind and unbind a handler scheme for the statements after them.
enum HandlerStatement<'a> {
    Alias(&'a str, &'a Comp),
    Unalias(&'a str),
}

impl<'a> HandlerStatement<'a> {
    /// `Ok(None)` when `comp` is neither statement; `Err` when it is one,
    /// malformed.
    fn of(comp: &'a Comp) -> Result<Option<Self>, TypeErrorKind> {
        let Some((head, exec)) = bare_call(comp) else {
            return Ok(None);
        };
        if !matches!(head, "alias" | "unalias") {
            return Ok(None);
        }
        let malformed = |detail| match head {
            "alias" => TypeErrorKind::MalformedAlias { detail },
            _ => TypeErrorKind::MalformedUnalias { detail },
        };
        if !exec.redirects.is_empty() {
            return Err(malformed("redirects are not allowed here"));
        }
        let Some(positional) = crate::ir::args::positional(&exec.args) else {
            return Err(malformed("spread arguments are not allowed here"));
        };
        match (head, &positional[..]) {
            ("alias", [Val::String(name), Val::Thunk(thunk)]) => {
                Ok(Some(Self::Alias(name.as_str(), thunk.shape())))
            }
            ("alias", _) => Err(malformed("expected `alias name { body }`")),
            (_, [Val::String(name)]) => Ok(Some(Self::Unalias(name.as_str()))),
            _ => Err(malformed("expected `unalias name`")),
        }
    }
}

/// Type-check a whole [`Toplevel`]: infer each phrase in order, extending
/// `TyEnv` at each `Define`, and binding/unbinding an `alias`/`unalias` `Run`
/// phrase's handler scheme for the phrases after it, as `Bind` on `Wildcard`
/// does for a nested discarded
/// statement.  Returns each `Define` phrase's generalised per-name
/// schemes, parallel to `top.phrases` and empty for every other phrase —
/// `annotate::annotate_toplevel` writes it straight onto the rebuilt
/// `Phrase::Define`, and the last phrase's computation type, which a contract
/// file's table is ascribed once inference has finished.
pub(crate) fn infer_toplevel(
    ctx: &mut InferCtx,
    env: &mut TyEnv,
    top: &Toplevel,
) -> (Vec<DefineSchemes>, Option<CompTy>) {
    Inferencer { ctx, env }.infer_phrases(&top.phrases)
}

/// Every `Name` an `IrPattern` binds, in pattern order — the phrase-level
/// analogue of [`Inferencer::bind_pattern`]'s own walk, kept separate since
/// this one only collects, never binds.
fn collect_pattern_names<'a>(pat: &'a IrPattern, out: &mut Vec<&'a str>) {
    match pat {
        IrPattern::Wildcard => {}
        IrPattern::Name(name) => out.push(name.as_ref()),
        IrPattern::List { elems, rest } => {
            for elem in elems {
                collect_pattern_names(elem, out);
            }
            if let Some(rest_name) = rest {
                out.push(rest_name.as_ref());
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
enum BindMode<'a> {
    /// `tied` is what the boundary left unquantified (`settle_pending_labels`).
    Let(&'a FreeVars),
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

    fn bind_pattern(&mut self, pat: &IrPattern, ty: &Ty, mode: BindMode<'_>) {
        match pat {
            IrPattern::Wildcard => {}
            IrPattern::Name(name) => {
                let scheme = match mode {
                    BindMode::Let(tied) => generalize(&self.ctx.unifier, self.env, tied, ty),
                    BindMode::Param => Scheme::mono(ty.clone()),
                };
                self.env.bind(name.to_string(), scheme);
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
                        BindMode::Let(tied) => {
                            generalize(&self.ctx.unifier, self.env, tied, &list_ty)
                        }
                        BindMode::Param => Scheme::mono(list_ty),
                    };
                    self.env.bind(rest_name.to_string(), scheme);
                }
            }
            IrPattern::Map(entries) => {
                let tail = Row::Var(self.ctx.unifier.fresh_row_var());
                let field_tys: Vec<Ty> = entries
                    .iter()
                    .map(|_| self.ctx.unifier.fresh_ty())
                    .collect();
                // A row nests tail-inward, so the first entry is wrapped last.
                let row =
                    entries
                        .iter()
                        .zip(&field_tys)
                        .rev()
                        .fold(tail, |row, (entry, field_ty)| {
                            Row::Extend(
                                Label::Field(entry.key.clone()),
                                Box::new(field_ty.clone()),
                                Box::new(row),
                            )
                        });
                self.ctx
                    .unify_ty(ty, &Ty::Record(row), Reason::RecordPattern);
                for (entry, field_ty) in entries.iter().zip(&field_tys) {
                    self.bind_pattern(&entry.pattern, field_ty, mode);
                }
            }
        }
    }

    /// Record `rhs`'s curried arity for `annotate`'s η-expansion, keyed
    /// by `rhs`'s own address — absent means "not `Fun`-shaped".
    fn record_arrow_arity(&mut self, rhs: &Comp, cty: &CompTy) {
        let arity = self.spine(cty).arity();
        if arity > 0 {
            self.ctx.rhs_arrow_arity.insert(comp_key(rhs), arity);
        }
    }

    /// Force `cty` to `Return` shape and read off what it returns.  A
    /// still-open comp var is unified into that shape rather than rejected —
    /// it may be a free var at an ungeneralized definition site — while a
    /// `Fun` fails under [`Reason::ReturnShape`].
    pub(super) fn extract_return(&mut self, cty: &CompTy) -> Ty {
        self.force_return_shape(cty, Reason::ReturnShape).ty
    }

    /// [`Self::extract_return`], reported under a caller-chosen [`Reason`] —
    /// the pipeline stage forcer wants its own hint, not the generic one —
    /// and keeping the grade.  A variable opens at a fresh grade: only the
    /// bind rule decides `p`.
    fn force_return_shape(&mut self, cty: &CompTy, why: Reason) -> Producer {
        if let Some(producer) = Producer::of(&self.ctx.unifier.resolve_comp_ty(cty)) {
            return producer;
        }
        let producer = self.fresh_producer();
        self.ctx.unify_comp_ty(cty, &producer.clone().into(), why);
        producer
    }

    fn fresh_producer(&mut self) -> Producer {
        Producer {
            grade: self.ctx.unifier.fresh_grade(),
            ty: self.ctx.unifier.fresh_ty(),
        }
    }

    /// A computation demanded ready to run — not a `Fun` still waiting for an
    /// argument — names the verb when it fails as a bare under-applied
    /// builtin, rather than reporting through the unifier as an anonymous
    /// mismatch.  Shared by a discarded value (extended to a non-tail `Seq`
    /// part and the program's own value) and a pipeline stage; `why` is the
    /// fallback [`Reason`] each wants for the ordinary shape mismatch.
    fn force_ready_shape(&mut self, comp: &Comp, cty: &CompTy, why: Reason) -> Producer {
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
                this.fresh_producer()
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
    /// type is its `rest`'s, all the way down, so `let a = 1; let b = 2;
    /// cd`'s discarded value is `cd`'s, not the outermost node's.
    fn discard_tail(comp: &Comp) -> &Comp {
        match &comp.item {
            CompKind::Bind { rest, .. } => Self::discard_tail(rest),
            _ => comp,
        }
    }

    /// `comp`'s own name and written argument count, when it is an `Exec`
    /// head resolving — by `exec_comp_ty`'s own lookup order — to a value
    /// builtin rather than a user binding.  The one case a discarded `Fun`
    /// should name as that verb's own arity error rather than an anonymous
    /// shape mismatch.
    fn discarded_builtin_arity(&self, comp: &Comp) -> Option<(String, usize, usize)> {
        let (name, exec) = bare_call(comp)?;
        let HeadClass::Value(entry) = self.head_class(name) else {
            return None;
        };
        let got = crate::ir::args::positional(&exec.args).map_or(0, |p| p.len());
        Some((name.to_string(), entry.fixed_arity(), got))
    }

    /// The span of the expression a computation's result comes from: its tail,
    /// through lambda bodies and the rest of each bind.
    pub(super) fn result_span(comp: &Comp) -> Option<Span> {
        match &comp.item {
            CompKind::Lam { body, .. } => Self::result_span(body),
            CompKind::Bind { rest, .. } => Self::result_span(rest),
            _ => comp.span,
        }
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
        let Some(span) = args.iter().find_map(|e| match e {
            ValListElem::Spread(v) => Some(v.span),
            ValListElem::Single(_) => None,
        }) else {
            return;
        };
        self.with_span(span, |this| {
            this.ctx
                .diagnose(TypeErrorKind::SpreadIntoApplication { head });
        });
    }

    pub(super) fn apply_args(&mut self, cty: CompTy, args: &crate::ir::Args) -> CompTy {
        self.apply_args_capped(cty, args, usize::MAX)
    }

    /// [`Self::apply_args`], applying no more than `cap` positionals: the
    /// surplus is still inferred, for the errors inside it, but unified
    /// against nothing — the same zip an over-applied builtin needs so its
    /// one arity diagnostic is not followed by an anonymous mismatch on the
    /// surplus.
    fn apply_args_capped(&mut self, mut cty: CompTy, args: &crate::ir::Args, cap: usize) -> CompTy {
        // A value takes its arguments by application, at an arity its own type
        // declares, so it has no argv and `...` has nothing to spread into.
        // Both callers are value-side, so the refusal needs no test on the head:
        // there is no such thing as an open-argv value for it to discriminate.
        // The subexpressions are still inferred, so errors inside them surface.
        let Some(positional) = crate::ir::args::positional(args) else {
            for slot in args.iter().map(ValListElem::slot) {
                self.with_span(slot.span, |this| {
                    let _ = this.infer_val(&slot.item);
                });
            }
            self.refuse_spread(args, super::error::SpreadHead::Applied);
            return self.peel_curry_spine(&cty);
        };
        // A block argument, its body held back until the whole spine is
        // unified, so a parameter a *later* argument determines — the element
        // type in `map { |x| … } $xs` — is known by the time the body reads it.
        struct Deferred<'a> {
            block: &'a Val,
            body: &'a Comp,
            wanted: CompTy,
            pos: Option<Span>,
        }
        let mut deferred = Vec::new();
        for (i, arg) in positional.into_iter().enumerate() {
            if i >= cap {
                let _ = self.infer_val(arg);
                continue;
            }
            cty = self.autoderef_thunk_return(cty);
            // Underline the offending argument, not the whole call.  A
            // synthetic entry carries no span, and `with_span` leaves pos alone.
            cty = self.with_span(args[i].slot().span, |this| {
                if let Val::Thunk(body) = arg {
                    let wanted = this.ctx.unifier.fresh_comp_ty();
                    deferred.push(Deferred {
                        block: arg,
                        body: body.shape(),
                        wanted: wanted.clone(),
                        pos: this.ctx.pos,
                    });
                    this.apply_to(&cty, Ty::Thunk(Box::new(wanted)))
                } else {
                    let ty = this.infer_val(arg);
                    this.apply_in_hand(&cty, arg, ty)
                }
            });
        }
        for Deferred {
            block,
            body,
            wanted,
            pos,
        } in deferred
        {
            self.with_span(pos, |this| {
                let given = this.with_scope(|this| this.check_comp(body, &wanted));
                match this.adapt(&given, &wanted) {
                    Some(how) => this.coerce(block, &given, how, &wanted, &Reason::Argument),
                    None => this.ctx.unify_comp_ty(&given, &wanted, Reason::Argument),
                }
            });
        }
        cty
    }

    /// `callee` applied to an argument of type `ty`: the residual computation.
    fn apply_to(&mut self, callee: &CompTy, ty: Ty) -> CompTy {
        let result = self.ctx.unifier.fresh_comp_ty();
        let expected = CompTy::Fun(Box::new(ty), Box::new(result.clone()));
        self.ctx.unify_comp_ty(callee, &expected, Reason::Argument);
        result
    }

    /// [`Self::apply_to`] for an argument in hand: a block adapted to the
    /// parameter's demand meets it there, η-wrapped at its arity if captured.
    fn apply_in_hand(&mut self, callee: &CompTy, arg: &Val, ty: Ty) -> CompTy {
        if let CompTy::Fun(param, body) = self.ctx.unifier.resolve_comp_ty(callee)
            && let Ty::Thunk(given) = self.ctx.unifier.resolve_ty(&ty)
            && let Ty::Thunk(wanted) = self.ctx.unifier.resolve_ty(&param)
            && let Some(how) = self.adapt(&given, &wanted)
        {
            self.coerce(arg, &given, how, &wanted, &Reason::Argument);
            return *body;
        }
        self.apply_to(callee, ty)
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
            return self.saturated_result(name, entry);
        };

        if positional.len() > fixed_arity {
            self.ctx.diagnose(match entry.diagnostic {
                BuiltinDiagnostic::Decoder if fixed_arity == 0 => {
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
        let head_cty = self.instantiate_comp(name, &scheme);
        self.apply_args_capped(head_cty, args, fixed_arity)
    }

    /// Peel `cty`'s whole curry spine, stopping at the first non-`Fun`: what a
    /// head's type becomes when nothing was applied and nothing is residual —
    /// a refused spread is the whole story of the call, so its type is the
    /// saturated result rather than the still-waiting arrow.
    fn peel_curry_spine(&self, cty: &CompTy) -> CompTy {
        self.spine(cty).tail
    }

    /// The `CompTy` a fresh instantiation of `entry`'s scheme names once all
    /// of it is (hypothetically) applied — what a refused spread into a
    /// builtin's type is.
    fn saturated_result(&mut self, name: &str, entry: &BuiltinEntry) -> CompTy {
        let scheme = (entry.type_rule)(&mut self.ctx.unifier);
        let cty = self.instantiate_comp(name, &scheme);
        self.peel_curry_spine(&cty)
    }

    /// The head's value type when it is concretely not a function — neither a
    /// `Thunk` nor a variable that could still become one.  Lets the `App` rule
    /// name `'foo' bar baz` before the general unifier mismatch fires.
    fn command_non_function_ty(&self, head_ty: &CompTy) -> Option<Ty> {
        match self.ctx.unifier.resolve_comp_ty(head_ty) {
            CompTy::Return(_, ty) => match self.ctx.unifier.resolve_ty(&ty) {
                Ty::Thunk(_) | Ty::Var(_) => None,
                concrete => Some(concrete),
            },
            CompTy::Fun(_, _) | CompTy::Var(_) => None,
        }
    }

    /// Instantiate `scheme`, strip its outer `Thunk`, and apply the body to
    /// `args`.  Instantiating here is what keeps quantified variables from
    /// being shared between call sites, so callers hand in a `Scheme` as is.
    pub(super) fn apply_scheme(
        &mut self,
        name: &str,
        scheme: &super::scheme::Scheme,
        args: &crate::ir::Args,
    ) -> CompTy {
        let head_cty = self.instantiate_comp(name, scheme);
        self.apply_args(head_cty, args)
    }

    /// Infer the arm for head `name`, check that it stands in for what the head
    /// is, and generalise.
    fn handler_comp_scheme(&mut self, name: &str, comp: &Comp) -> Scheme {
        let cty = self.infer_handler_comp(comp);
        self.install_arm_scheme(name, &cty)
    }

    /// The scheme a `handlers:` arm is bound at.  An arm written out is
    /// *inferred* under the argv convention, so its body is checked against
    /// the parameters its call sites will hand it; an arm in hand as a value
    /// was inferred under the ordinary one, whose parameter is a fresh
    /// variable, and unification imposes the convention after the fact.  So
    /// the two agree on the arm's type and the written one is stronger on its
    /// body.
    pub(super) fn handler_arm_scheme(&mut self, name: &str, arm: &Val) -> Scheme {
        match arm {
            Val::Thunk(comp) => self.handler_comp_scheme(name, comp.shape()),
            value => {
                let ty = self.infer_val(value);
                let cty = self.ctx.unifier.fresh_comp_ty();
                self.ctx
                    .unify_ty(&ty, &Ty::Thunk(Box::new(cty.clone())), Reason::HandlerArm);
                self.pin_arm_params_to_argv(&cty);
                self.install_arm_scheme(name, &cty)
            }
        }
    }

    /// Pin `cty` to what head `name` stands for and generalise — the tail both
    /// spellings of an arm share.  A failure is an ordinary positioned error.
    fn install_arm_scheme(&mut self, name: &str, cty: &CompTy) -> Scheme {
        let cty = match self.stands_in(name, cty) {
            Ok(cty) => cty,
            Err((cty, error)) => {
                self.ctx.raise(*error);
                cty
            }
        };
        let thunk_ty = Ty::Thunk(Box::new(cty));
        let tied = self.ctx.settle_pending_labels(Some(self.env));
        generalize(&self.ctx.unifier, self.env, &tied, &thunk_ty)
    }

    /// What head `name` stands for, as `(arm)` compares an arm against it: the
    /// type of the base frame or arm already in force under `name`, and a
    /// command for anything else — an external, or a value row's name, whose
    /// arm is dead because the table is searched first.
    fn stands_for(&mut self, name: &str) -> (CompTy, Standing) {
        match self.env.lookup_handler(name).cloned() {
            Some(handler) => (
                self.instantiate_comp(name, &handler.scheme),
                Standing::Own(name.to_string()),
            ),
            None => (command_head(), Standing::Command(name.to_string())),
        }
    }

    /// `(arm)`: an arm produces what the head it stands in for produces, and
    /// is installed at the head's producer kind.  Refused as an error ready to
    /// raise, with the arm as it was, since the run-time door that vets an
    /// installed arm renders it in its own words.
    pub(super) fn stands_in(
        &mut self,
        name: &str,
        arm: &CompTy,
    ) -> Result<CompTy, (CompTy, Box<TypeError>)> {
        let (head, standing) = self.stands_for(name);
        self.arm_stands_in(arm, &head, standing)
    }

    /// The catch-all stands in for every command.
    pub(super) fn catch_all_stands_in(
        &mut self,
        arm: &CompTy,
    ) -> Result<CompTy, (CompTy, Box<TypeError>)> {
        self.arm_stands_in(arm, &command_head(), Standing::EveryCommand)
    }

    /// Unify what `arm` and `head` produce, each past its parameters: an arm
    /// written without one discards its argv, as the runtime does.  A `()`
    /// producer stands in for a command: its output is whatever it wrote.
    fn arm_stands_in(
        &mut self,
        arm: &CompTy,
        head: &CompTy,
        standing: Standing,
    ) -> Result<CompTy, (CompTy, Box<TypeError>)> {
        let wanted = self.producer_of(head);
        let found = self.producer_of(arm);
        let upcast = matches!(
            (&wanted, &found),
            (CompTy::Return(Grade::Output, _), CompTy::Return(Grade::Value, unit))
                if self.is_unit(unit)
        );
        let verdict = if upcast {
            Ok(())
        } else {
            self.ctx.try_unify_comp_ty(&wanted, &found)
        };
        // Installed at the head's producer either way, so a refused arm is
        // refused once, not again at every call.
        let wanted = self.ctx.unifier.apply_comp_ty(&wanted);
        let installed = self.map_producer(arm, |_| wanted);
        match verdict {
            Ok(()) => Ok(installed),
            Err(kind) => {
                let error = TypeError {
                    pos: self.ctx.pos,
                    kind,
                    reason: Some(Reason::StandsIn(standing)),
                    weak: None,
                    unit: None,
                };
                Err((installed, Box::new(error)))
            }
        }
    }

    /// What `cty` produces once every parameter is supplied, forced to
    /// `Return` shape.
    fn producer_of(&mut self, cty: &CompTy) -> CompTy {
        let tail = self.spine(cty).tail;
        self.force_return_shape(&tail, Reason::ReturnShape).into()
    }

    /// Force the argv convention on an arm's parameters.  A written arm is
    /// *inferred* under it ([`Self::infer_alias_arm`]); an arm in hand as a
    /// value has its own parameter type already, and only unification can
    /// impose the convention on that.
    fn pin_arm_params_to_argv(&mut self, cty: &CompTy) {
        if let CompTy::Fun(param, body) = self.ctx.unifier.resolve_comp_ty(cty) {
            self.ctx.unify_ty(&param, &Ty::argv(), Reason::AliasParam);
            self.pin_arm_params_to_argv(&body);
        }
    }

    /// Instantiate the scheme head `name` is bound at and strip the outer
    /// `Thunk` it carries.  A head still a variable is a thunk by use; one
    /// bound to anything else is no program.
    fn instantiate_comp(&mut self, name: &str, scheme: &Scheme) -> CompTy {
        let ty = self.ctx.instantiate(scheme);
        match self.ctx.unifier.resolve_ty(&ty) {
            Ty::Thunk(body) => *body,
            Ty::Var(_) => {
                let body = self.ctx.unifier.fresh_comp_ty();
                self.ctx.unify_ty(
                    &ty,
                    &Ty::Thunk(Box::new(body.clone())),
                    Reason::AutoderefHead,
                );
                body
            }
            ty => {
                self.ctx.diagnose(TypeErrorKind::HeadBoundToValue {
                    name: name.to_string(),
                    ty,
                });
                self.ctx.unifier.fresh_comp_ty()
            }
        }
    }

    /// Apply an alias/handler arm — a user's or a base frame's — to a call
    /// site's arguments.  A parameterised arm is `Fun(argv, body)`, and the
    /// argv rule says what that parameter is; a nullary arm discards its
    /// arguments, as the runtime does.
    fn apply_alias_arm(&mut self, name: &str, scheme: &Scheme, args: &crate::ir::Args) -> CompTy {
        let cty = self.instantiate_comp(name, scheme);
        let argv = self.argv_ty(args, ArgvBoundary::InShell(name));
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

    /// `comp` against a type already known: a block whose parameter the
    /// expectation names binds *that* type rather than a fresh one, so the
    /// body reads a parameter its own call site determined.  Anything else has
    /// nothing to push inwards and is inferred as ever, the caller unifying.
    pub(super) fn check_comp(&mut self, comp: &Comp, expected: &CompTy) -> CompTy {
        match (&comp.item, self.ctx.unifier.resolve_comp_ty(expected)) {
            (CompKind::Lam { param, body }, CompTy::Fun(param_ty, result)) => {
                let body_ty = self.with_scope(|this| {
                    this.bind_pattern(param, &param_ty, BindMode::Param);
                    this.check_comp(body, &result)
                });
                CompTy::Fun(param_ty, Box::new(body_ty))
            }
            _ => self.infer_comp(comp),
        }
    }

    fn infer_handler_comp(&mut self, comp: &Comp) -> CompTy {
        match &comp.item {
            CompKind::Lam { param, body } => self.infer_alias_arm(Some(param), body),
            _ => self.infer_alias_arm(None, comp),
        }
    }

    /// The catch-all convention: `Fun(String, Fun(List String, body))`. The
    /// name is bound `String`; the argv layer and body reuse
    /// [`Self::infer_handler_comp`], so the shape is exactly a per-name arm
    /// nested one level under the name.
    pub(super) fn infer_catch_all(&mut self, comp: &Comp) -> CompTy {
        let (name_param, rest) = match &comp.item {
            CompKind::Lam { param, body } => (Some(param), body.as_ref()),
            // arity is vetted elsewhere; infer the body for its errors
            _ => (None, comp),
        };
        self.with_scope(|this| {
            if let Some(name_param) = name_param {
                this.bind_pattern(name_param, &Ty::String, BindMode::Param);
            }
            let body_cty = this.infer_handler_comp(rest);
            CompTy::Fun(Box::new(Ty::String), Box::new(body_cty))
        })
    }

    fn infer_not(&mut self, val: &Val) -> Ty {
        let ty = self.infer_val(val);
        self.ctx.unify_ty(&ty, &Ty::Bool, Reason::NotOperand);
        Ty::Bool
    }

    fn infer_binary(&mut self, op: BinaryOp, lhs: &Val, rhs: &Val) -> Ty {
        let lhs_ty = self.infer_val(lhs);
        let rhs_ty = self.infer_val(rhs);
        let why = Reason::BinaryOperands(op.kind());
        let operand = match op.kind() {
            BinaryOpKind::Arith(_) => self.ctx.fresh_kinded(Kind::NUMBER),
            BinaryOpKind::Compare(_) => self.ctx.fresh_kinded(Kind::COMPARABLE),
            BinaryOpKind::Eq(_) => self.ctx.fresh_kinded(Kind::DATA),
        };
        self.ctx.unify_ty(&operand, &lhs_ty, why.clone());
        self.ctx.unify_ty(&operand, &rhs_ty, why.clone());
        if op.kind() == BinaryOpKind::Arith(ArithOp::Mod) {
            self.ctx.unify_ty(&operand, &Ty::Int, why);
        }
        match op.kind() {
            BinaryOpKind::Eq(_) | BinaryOpKind::Compare(_) => Ty::Bool,
            BinaryOpKind::Arith(_) => operand,
        }
    }

    /// A bare command head's type, by the lookup order the runtime uses —
    /// binding, value builtin, handler, external.  A binding hit is final, and
    /// a pristine native reaches here only through the rule table, the
    /// bindings harvest walking user scopes alone.  `^name` and a path head
    /// never reach this rule — they are the external directly
    /// (`external_exec_comp_ty`), consulting neither the env nor the stack.
    ///
    /// The four arms are ral's two worlds in order.  The first two are lambda
    /// calculus: arguments by application, at an arity the head's own type
    /// declares, so `...` has no argv to spread into and is refused.  The last
    /// two take an argv, and `...` is exactly its notation.
    fn exec_comp_ty(&mut self, comp: &Comp, name: &str, args: &crate::ir::Args) -> CompTy {
        match self.head_class(name) {
            HeadClass::Binding(scheme) => self.apply_scheme(name, &scheme, args),
            HeadClass::Value(entry) => {
                let cty = self.apply_builtin(&entry, name, args);
                self.note_boundary_call(comp, &entry, args, &cty);
                cty
            }
            HeadClass::Arm(handler) => self.apply_alias_arm(name, &handler.scheme, args),
            // Prelude functions arrive as an `App` on a bound variable, never
            // as a bare `Exec` head.
            HeadClass::External => self.external_exec_comp_ty(name, args),
        }
    }

    fn head_class(&self, name: &str) -> HeadClass {
        if let Some(scheme) = self.env.lookup_binding(name) {
            return HeadClass::Binding(Arc::clone(scheme));
        }
        if let Some(entry) = self.env.builtins.value(name) {
            return HeadClass::Value(entry);
        }
        if let Some(handler) = self.env.lookup_handler(name) {
            return HeadClass::Arm(handler.clone());
        }
        HeadClass::External
    }

    /// A call of a boundary row: its result is weak, and the node gets a site.
    fn note_boundary_call(
        &mut self,
        comp: &Comp,
        entry: &BuiltinEntry,
        args: &crate::ir::Args,
        cty: &CompTy,
    ) {
        if !entry.is_boundary() {
            return;
        }
        let Some(producer) = self.spine(cty).producer() else {
            return;
        };
        let key = comp_key(comp);
        self.ctx.record_boundary(key, &entry.name, producer.ty);
        let arity = entry.fixed_arity();
        let given = crate::ir::args::positional(args).map_or(arity, |given| given.len());
        if given < arity {
            self.ctx.boundary_missing.insert(key, arity - given);
        }
    }

    /// An external takes an argv and is a command: its value is its output.
    ///
    /// Nothing here declares a parameter for that argv to meet, so the rule's
    /// contribution is its walk and its refusals rather than its type.
    fn external_exec_comp_ty(&mut self, shown: &str, args: &crate::ir::Args) -> CompTy {
        let _argv = self.argv_ty(args, ArgvBoundary::Exec(shown));
        CompTy::command()
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
            self.with_span(entry.slot().span, |this| match entry {
                ValListElem::Single(arg) => {
                    let ty = this.infer_val(&arg.item);
                    this.gate_arg(&ty, boundary);
                }
                // A spread contributes as many elements as the list holds, and
                // how many that is only the run knows: an empty one contributes
                // none, and refuses nothing.  So its elements are left to the
                // spawn-time gate, which counts them.
                ValListElem::Spread(arg) => {
                    let spread_ty = this.infer_val(&arg.item);
                    let elem = this.ctx.unifier.fresh_ty();
                    this.ctx
                        .unify_ty(&spread_ty, &Ty::List(Box::new(elem)), Reason::ListSpread);
                }
            });
        }
        Ty::argv()
    }

    /// Refuse an argv element the boundary has no argument for, one step
    /// before the spawn that would refuse it — which is what makes the promise
    /// that argument-type errors are reported before execution true of an
    /// external's arguments too.
    ///
    /// A concrete type only.  Where the type is still a variable the shape is
    /// genuinely unknown here, and `runtime::command::vet` keeps the question:
    /// nothing that runs today stops running, and the two answers come from one
    /// declaration ([`RefusedArg`]) rather than from two matches that might
    /// drift.
    fn gate_arg(&mut self, ty: &Ty, boundary: ArgvBoundary<'_>) {
        // The head decides the verdict; the message shows the whole type.
        let head = self.ctx.unifier.resolve_ty(ty);
        let refusal = RefusedArg::of_ty(&head);
        let command = match boundary {
            ArgvBoundary::Exec(command) if refusal.is_some() => command,
            ArgvBoundary::InShell(command) if refusal == Some(RefusedArg::Unit) => command,
            _ => return,
        };
        let ty = self.ctx.unifier.apply_ty(&head);
        self.ctx.diagnose(TypeErrorKind::ExecArgNotText {
            command: command.to_string(),
            ty,
        });
    }

    /// Infer every argument for the errors inside it, constraining nothing —
    /// what a refused call still owes its subexpressions.
    pub(super) fn infer_refused_args(&mut self, args: &crate::ir::Args) {
        for slot in args.iter().map(ValListElem::slot) {
            self.with_span(slot.span, |this| {
                let _ = this.infer_val(&slot.item);
            });
        }
    }

    /// [`infer_toplevel`]'s walk: one phrase at a time, each under its own
    /// span, threading the extended `TyEnv` from one phrase to the next.
    ///
    /// The toplevel is a sequence, so it has no type of its own; what it has
    /// is a value, and that value is its last `Run`'s.  The returned `CompTy`
    /// is `None` when the last phrase is a `Define` — a file that ends
    /// without producing a value.
    fn infer_phrases(
        &mut self,
        phrases: &[Spanned<Phrase>],
    ) -> (Vec<DefineSchemes>, Option<CompTy>) {
        let mut schemes = Vec::with_capacity(phrases.len());
        let mut tail = None;
        for phrase in phrases {
            let (names, cty) = self.with_span(phrase.span, |this| this.infer_phrase(&phrase.item));
            schemes.push(names);
            tail = cty;
        }
        (schemes, tail)
    }

    /// One phrase.  Every `Run`'s value is held to the discarded shape, the
    /// tail's included — its own writes are never captured into the report.
    /// Only a `Run` has a value, so only a `Run` hands a `CompTy` back.
    fn infer_phrase(&mut self, phrase: &Phrase) -> (DefineSchemes, Option<CompTy>) {
        match phrase {
            Phrase::Define { pattern, comp, .. } => {
                self.infer_let(pattern, comp);
                let mut names = Vec::new();
                collect_pattern_names(pattern, &mut names);
                let schemes = names
                    .into_iter()
                    .map(|name| {
                        let scheme = self
                            .env
                            .lookup_binding(name)
                            .cloned()
                            .expect("bind_pattern just bound every collected name");
                        (name.to_string(), scheme)
                    })
                    .collect();
                (schemes, None)
            }
            Phrase::Run(comp) => (Vec::new(), Some(self.infer_statement(comp))),
        }
    }

    /// `let pattern = rhs`: the RHS inferred as the holder of the indexes it
    /// reads, bound by the bind rule, and generalised.
    fn infer_let(&mut self, pattern: &IrPattern, rhs: &Comp) {
        let cty = self.infer_held(pattern, rhs);
        self.record_arrow_arity(rhs, &cty);
        let bound_ty = self.rhs_bound_ty(rhs, cty);
        let tied = self.ctx.settle_pending_labels(Some(self.env));
        let concrete = self.ctx.unifier.apply_ty_keeping_weak(&bound_ty);
        self.bind_pattern(pattern, &concrete, BindMode::Let(&tied));
        self.note_called(pattern, rhs);
    }

    /// A discarded statement: an `alias`/`unalias` binds or unbinds its
    /// handler scheme for what follows, and anything else is inferred and
    /// held to the discarded shape.
    fn infer_statement(&mut self, comp: &Comp) -> CompTy {
        let handled = match HandlerStatement::of(comp) {
            Ok(Some(HandlerStatement::Alias(name, body))) => {
                let scheme = self.handler_comp_scheme(name, body);
                self.env.bind_handler(name.to_string(), scheme, true);
                true
            }
            Ok(Some(HandlerStatement::Unalias(name))) => {
                self.env.unbind_removable_handler(name);
                true
            }
            Ok(None) => false,
            Err(kind) => {
                self.ctx.diagnose(kind);
                true
            }
        };
        let cty = if handled {
            CompTy::pure(Ty::Unit)
        } else {
            self.infer_comp(comp)
        };
        self.force_discarded_shape(comp, &cty);
        cty
    }

    /// Whether `key` repeats a label `existing` already yields — diagnosing
    /// T0022 (`TypeErrorKind::DuplicateField`) if so.  Shared by a record's
    /// fields and a map's static keys: SPEC §4.5 refuses either duplicate at
    /// check time.
    fn diagnose_if_duplicate<'a>(
        &mut self,
        key: &str,
        existing: impl Iterator<Item = &'a str>,
    ) -> bool {
        let duplicate = existing.into_iter().any(|seen| seen == key);
        if duplicate {
            self.ctx.diagnose(TypeErrorKind::DuplicateField {
                label: key.to_string(),
            });
        }
        duplicate
    }

    /// `fields`, folded into a row over `tail`, last-declared innermost —
    /// shared by a plain record's closed row and a spread-bearing one's update
    /// over its base's tail.
    fn fields_row(fields: Vec<(String, Ty)>, tail: Row) -> Row {
        fields.into_iter().rev().fold(tail, |rest, (key, ty)| {
            Row::Extend(Label::Field(key), Box::new(ty), Box::new(rest))
        })
    }

    /// One record field, folded into `fields` unless its label repeats
    /// (diagnosed there and then dropped: one label, one slot).  Shared by a plain
    /// record and one with a spread ([`Assembly::Record`]).
    fn infer_record_field(
        &mut self,
        key: &str,
        value: &Spanned<Val>,
        fields: &mut Vec<(String, Ty)>,
    ) {
        let (duplicate, ty) = self.with_span(value.span, |this| {
            let duplicate = this.diagnose_if_duplicate(key, fields.iter().map(|(k, _)| k.as_str()));
            let ty = this.infer_val(&value.item);
            (duplicate, ty)
        });
        if !duplicate {
            fields.push((key.to_string(), ty));
        }
    }

    /// A plain record literal: no spread, so one row, closed.
    fn infer_record_val(&mut self, entries: &[(Name, Spanned<Val>)]) -> Ty {
        let mut fields: Vec<(String, Ty)> = Vec::new();
        for (key, value) in entries {
            self.infer_record_field(key, value, &mut fields);
        }
        Ty::Record(Self::fields_row(fields, Row::Empty))
    }

    /// A record literal with a spread ([`Assembly::Record`]): the update rule.
    /// The base is demanded to have a slot at each written label, at a type of
    /// its own that nothing imposes; the result keeps the base's tail and gives
    /// each written label the type written.  The parser admits one spread, first,
    /// so the literal shares one tail.
    fn infer_assembly_record(&mut self, entries: &[ValRecordEntry]) -> Ty {
        let mut fields: Vec<(String, Ty)> = Vec::new();
        let mut base = None;
        for entry in entries {
            match entry {
                ValRecordEntry::Field(key, value) => {
                    self.infer_record_field(key, value, &mut fields);
                }
                ValRecordEntry::Spread(value) => {
                    let ty = self.with_span(value.span, |this| this.infer_val(&value.item));
                    base = Some((value, ty));
                }
            }
        }
        let tail = self.ctx.unifier.fresh_row();
        if let Some((value, base_ty)) = base {
            let probe = fields.iter().rev().fold(tail.clone(), |rest, (key, _)| {
                let old = self.ctx.unifier.fresh_ty();
                Row::Extend(Label::Field(key.clone()), Box::new(old), Box::new(rest))
            });
            let reason = Reason::RecordUpdate {
                base: match &value.item {
                    Val::Variable(name) if !is_gensym(name) => Some(name.to_string()),
                    _ => None,
                },
            };
            self.with_span(value.span, |this| {
                this.ctx.unify_ty(&base_ty, &Ty::Record(probe), reason);
            });
        }
        Ty::Record(Self::fields_row(fields, tail))
    }

    /// A plain map literal: `Map<elem>`, every key a static label — so the
    /// checker refuses a static duplicate exactly as a record's (T0022).
    fn infer_map_val(&mut self, entries: &[(Name, Spanned<Val>)]) -> Ty {
        let elem = self.ctx.unifier.fresh_ty();
        let mut seen: Vec<Name> = Vec::new();
        for (key, value) in entries {
            let duplicate = self.with_span(value.span, |this| {
                let duplicate = this.diagnose_if_duplicate(key, seen.iter().map(AsRef::as_ref));
                let value_ty = this.infer_val(&value.item);
                this.ctx.unify_ty(&value_ty, &elem, Reason::MapElem);
                duplicate
            });
            if !duplicate {
                seen.push(key.clone());
            }
        }
        Ty::Map(Box::new(elem))
    }

    /// A map literal with a computed key or a spread ([`Assembly::Map`]):
    /// `Map<elem>`, one `elem` shared by every value and spread. A static
    /// label (`key:`, as opposed to `$k:`) is checked for a duplicate exactly
    /// as [`Self::infer_map_val`]'s are — SPEC §4.5 makes any *static*
    /// duplicate an error; only a computed key is left to the runtime
    /// warning.
    fn infer_assembly_map(&mut self, entries: &[ValMapEntry]) -> Ty {
        // Keys must be `String`: the runtime's status-1 refusal, lifted here.
        let elem = self.ctx.unifier.fresh_ty();
        let mut seen: Vec<String> = Vec::new();
        for entry in entries {
            match entry {
                ValMapEntry::Entry(key, value) => {
                    let key_ty = self.infer_val(key);
                    self.ctx.unify_ty(&key_ty, &Ty::String, Reason::MapKey);
                    self.with_span(value.span, |this| {
                        if let Val::String(label) = key {
                            this.diagnose_if_duplicate(
                                label.as_str(),
                                seen.iter().map(String::as_str),
                            );
                        }
                        let value_ty = this.infer_val(&value.item);
                        this.ctx.unify_ty(&value_ty, &elem, Reason::MapElem);
                    });
                    if let Val::String(label) = key {
                        seen.push(label.as_str().to_string());
                    }
                }
                ValMapEntry::Spread(value) => {
                    self.with_span(value.span, |this| {
                        let spread_ty = this.infer_val(&value.item);
                        this.ctx.unify_ty(
                            &spread_ty,
                            &Ty::Map(Box::new(elem.clone())),
                            Reason::MapSpread,
                        );
                    });
                }
            }
        }
        Ty::Map(Box::new(elem))
    }

    /// [`Assembly`], typed by its arm — the moved literal code, over a
    /// computed key or a spread.
    fn infer_assembly(&mut self, assembly: &Assembly) -> Ty {
        match assembly {
            Assembly::List(elems) => self.infer_assembly_list(elems),
            Assembly::Record(entries) => self.infer_assembly_record(entries),
            Assembly::Map(entries) => self.infer_assembly_map(entries),
        }
    }

    /// One list element's contribution to `elem`, the list's shared element
    /// type: a plain value unifies directly, a spread's own element type
    /// does. Shared by a plain list and one with a spread
    /// ([`Assembly::List`]).
    fn infer_list_entry(&mut self, value: &Spanned<Val>, spread: bool, elem: &Ty) {
        self.with_span(value.span, |this| {
            let entry_ty = if spread {
                let spread_ty = this.infer_val(&value.item);
                let inner = this.ctx.unifier.fresh_ty();
                this.ctx.unify_ty(
                    &spread_ty,
                    &Ty::List(Box::new(inner.clone())),
                    Reason::ListSpread,
                );
                inner
            } else {
                this.infer_val(&value.item)
            };
            this.ctx.unify_ty(&entry_ty, elem, Reason::ListElem);
        });
    }

    /// A list literal with a spread ([`Assembly::List`]).
    fn infer_assembly_list(&mut self, elems: &[ValListElem]) -> Ty {
        let elem = self.ctx.unifier.fresh_ty();
        for entry in elems {
            match entry {
                ValListElem::Single(v) => self.infer_list_entry(v, false, &elem),
                ValListElem::Spread(v) => self.infer_list_entry(v, true, &elem),
            }
        }
        Ty::List(Box::new(elem))
    }

    /// `$name` is bound nowhere; the nearest names that are — bindings and
    /// value builtins, never handlers — are its likely intents.
    fn diagnose_unbound(&mut self, name: &str) {
        let candidates = self
            .env
            .binding_names()
            .chain(self.env.builtins.value_names())
            .chain(LANGUAGE_CONSTANTS.iter().map(|&(name, _)| name));
        let suggestions = crate::text::near_names(name, candidates, 3)
            .into_iter()
            .map(str::to_string)
            .collect();
        self.ctx.diagnose(TypeErrorKind::UnboundVariable {
            name: name.to_string(),
            suggestions,
        });
    }

    /// A builtin held as a value.  A boundary's result is weak here too, and
    /// `annotate` rebuilds the reference into the block holding its checked
    /// call, so a value is admitted like any call.
    fn builtin_value(&mut self, val: &Val, entry: &BuiltinEntry) -> Ty {
        let scheme = (entry.type_rule)(&mut self.ctx.unifier);
        let ty = self.ctx.instantiate(&scheme);
        if entry.is_boundary()
            && let Ty::Thunk(body) = &ty
            && let Some(producer) = self.spine(body).producer()
        {
            let key = val_key(val);
            self.ctx.record_boundary(key, &entry.name, producer.ty);
            self.ctx.boundary_values.insert(
                key,
                BoundaryValue {
                    name: entry.name.to_string(),
                    arity: entry.fixed_arity(),
                },
            );
        }
        ty
    }

    pub(super) fn infer_val(&mut self, val: &Val) -> Ty {
        match val {
            Val::Unit => Ty::Unit,
            Val::String(_) => Ty::String,
            Val::Int(_) => Ty::Int,
            Val::Float(_) => Ty::Float,
            Val::Bool(_) => Ty::Bool,
            Val::Variable(name) => {
                if ScopeAst::lookup_keyword(name).is_some() {
                    self.ctx.diagnose(TypeErrorKind::ControlOperatorAsValue {
                        name: name.to_string(),
                    });
                    self.ctx.unifier.fresh_ty()
                } else {
                    match self.env.lookup_binding(name).cloned() {
                        Some(scheme) => {
                            let ty = self.ctx.instantiate(&scheme);
                            self.ctx.note_residual_use(name, &scheme, &ty);
                            self.note_unit_read(name, &ty);
                            ty
                        }
                        None => match self.env.builtins.value(name) {
                            Some(entry) => self.builtin_value(val, &entry),
                            // A base frame is a builtin *and* a handler:
                            // name the builtin, which is what the user
                            // wrote and what `explain` documents.
                            None if self.env.builtins.get(name).is_some() => {
                                self.ctx.diagnose(TypeErrorKind::BuiltinNotFirstClass {
                                    name: name.to_string(),
                                });
                                self.ctx.unifier.fresh_ty()
                            }
                            None if self.env.lookup_handler(name).is_some() => {
                                self.ctx.diagnose(TypeErrorKind::HandlerNotFirstClass {
                                    name: name.to_string(),
                                });
                                self.ctx.unifier.fresh_ty()
                            }
                            None if LANGUAGE_CONSTANTS.iter().any(|&(c, _)| c == &**name) => {
                                Ty::Bool
                            }
                            None => {
                                self.diagnose_unbound(name);
                                self.ctx.unifier.fresh_ty()
                            }
                        },
                    }
                }
            }
            Val::Thunk(comp) => Ty::Thunk(Box::new(
                self.with_scope(|this| this.infer_comp(comp.shape())),
            )),
            Val::List(elems) => {
                let elem = self.ctx.unifier.fresh_ty();
                for entry in elems.shape() {
                    self.infer_list_entry(entry, false, &elem);
                }
                Ty::List(Box::new(elem))
            }
            Val::Record(entries) => self.infer_record_val(entries.shape()),
            Val::Map(entries) => self.infer_map_val(entries.shape()),
            Val::Variant { label, payload } => {
                // Construction is open: `` `ok 5 `` gets a fresh row tail.
                let payload_ty = match payload {
                    Some(p) => self.infer_val(p),
                    None => Ty::Unit,
                };
                let rest = self.ctx.unifier.fresh_row();
                Ty::Variant(Row::Extend(
                    Label::Case(label.to_string()),
                    Box::new(payload_ty),
                    Box::new(rest),
                ))
            }
        }
    }

    /// A `|` is a positional operating-system byte wire: a stage feeds the
    /// next by writing, so every stage but the last produces `()`, at any
    /// grade, and is no decoder ([`Self::stage_writes`]); the pipeline
    /// produces what its final stage does.  Its shape is its own: a stage must
    /// be a computation ready to run, not a `Fun` still waiting for its
    /// argument, forced under
    /// its own [`Reason::PipelineStageShape`] rather than
    /// [`Self::extract_return`]'s generic one, to earn the shape's own hint
    /// text.  Its stdin belongs to the wire: a stage after a `|` may not bind
    /// standard input at its own root, since the feed would answer every read
    /// it makes and leave the producer working for nobody
    /// ([`stage_root_stdin_feed`]).
    fn infer_pipeline(&mut self, stages: &[Arc<Comp>]) -> CompTy {
        // The parser unwraps a single-stage pipeline to the bare stage and the
        // elaborator preserves that shape, so a `Pipeline` node always has two.
        debug_assert!(stages.len() >= 2, "Pipeline carries ≥2 stages");

        let mut last = None;
        for (position, stage) in stages.iter().enumerate() {
            if position > 0
                && let Some(feed) = stage_root_stdin_feed(stage)
            {
                self.with_span(stage.span, |this| {
                    this.ctx.diagnose(TypeErrorKind::DeadPipeEdge { feed });
                });
            }
            let cty = self.infer_comp(stage);
            let producer = self.with_span(stage.span, |this| {
                this.force_ready_shape(stage, &cty, Reason::PipelineStageShape)
            });
            if let Some(next) = stages.get(position + 1)
                && !matches!(self.ctx.unifier.resolve_comp_ty(&cty), CompTy::Fun(..))
            {
                self.with_span(stage.span, |this| {
                    this.stage_writes(stage, next, &producer);
                });
            }
            self.ctx
                .stage_types
                .insert(comp_key(stage), producer.ty.clone());
            last = Some(producer.into());
        }
        // The pipeline produces what its last stage does, at its own grade.
        last.expect("≥2 stages by invariant above")
    }

    /// `(pipe)` for a stage before the last, `producer`: it feeds `next` by
    /// writing, so its value is `()`, whatever its grade.  A decoder is
    /// refused by its own mark rather than by its type: `∀α. F α` would unify
    /// `α := Unit` and let the failure wait for the decode.
    fn stage_writes(&mut self, stage: &Comp, next: &Comp, producer: &Producer) {
        if let Some(name) = self.stage_decoder(stage) {
            return self.ctx.diagnose(TypeErrorKind::DecoderMidPipeline {
                name,
                next: Self::stage_head(next),
            });
        }
        let why = Reason::PipelineStageWrites {
            stage: Self::stage_head(stage),
            next: Self::stage_head(next),
        };
        let unit = Producer {
            grade: producer.grade,
            ty: Ty::Unit,
        };
        self.ctx
            .unify_comp_ty(&producer.clone().into(), &unit.into(), why);
    }

    /// The decoder `stage` runs, past the binds hoisted around it.
    fn stage_decoder(&self, stage: &Comp) -> Option<String> {
        let (name, _) = bare_call(Self::discard_tail(stage))?;
        match self.head_class(name) {
            HeadClass::Value(entry) if entry.diagnostic == BuiltinDiagnostic::Decoder => {
                Some(name.to_string())
            }
            _ => None,
        }
    }

    /// The name of the command a stage runs, when it is written as one.
    fn stage_head(stage: &Comp) -> Option<String> {
        match &Self::discard_tail(stage).item {
            CompKind::Exec(exec) => Some(exec.head.name().written().into_owned()),
            CompKind::Force(Val::Variable(name)) => Some(name.to_string()),
            CompKind::App { head, .. } => match &head.item {
                CompKind::Force(Val::Variable(name)) => Some(name.to_string()),
                _ => None,
            },
            _ => None,
        }
    }

    fn infer_index(&mut self, target: &Val, keys: &[crate::source::Spanned<Val>]) -> CompTy {
        if let (Some(literal), Some(first)) = (spell_literal(target), keys.first()) {
            for key in keys {
                self.infer_val(&key.item);
            }
            self.ctx.diagnose(TypeErrorKind::IndexOnLiteral {
                literal,
                key: spell_key(&first.item),
            });
            return CompTy::pure(self.ctx.unifier.fresh_ty());
        }
        let mut current_ty = self.infer_val(target);
        for (step, key) in keys.iter().enumerate() {
            let names = match (step, target, &key.item) {
                (0, Val::Variable(t), Val::Variable(k)) if !is_gensym(t) && !is_gensym(k) => {
                    Some((t.to_string(), k.to_string()))
                }
                _ => None,
            };
            current_ty = self.with_span(key.span, |this| {
                this.infer_index_step(&current_ty, &key.item, names)
            });
        }
        CompTy::pure(current_ty)
    }

    /// One step of an indexing chain, run under the pos `infer_index` narrowed
    /// to this key, so a failure underlines the step and not the whole chain.
    /// The key's form decides the rule before the target's type is read, so an
    /// inline read and the same read extracted into a block agree: a bare
    /// label is a [`Lbl`], and any other key an [`Idx`], whose three types are
    /// weak for the unit unless the key is an integer literal, which is the
    /// list rule and waits for nothing.
    fn infer_index_step(
        &mut self,
        current_ty: &Ty,
        key: &Val,
        names: Option<(String, String)>,
    ) -> Ty {
        if let Val::String(label) = key {
            return self.infer_label_read(current_ty, label);
        }
        let idx = Idx {
            target: current_ty.clone(),
            key: self.infer_val(key),
            elem: self.ctx.unifier.fresh_ty(),
            span: self.ctx.pos,
            names,
            holder: self.ctx.holder,
        };
        if !matches!(key, Val::Int(_)) {
            for ty in [&idx.target, &idx.key, &idx.elem] {
                self.ctx.unifier.mark_weak(ty, &WeakSource::Index);
            }
        }
        let elem = idx.elem.clone();
        if matches!(self.ctx.settle_index(&idx), Settled::Pending) {
            self.ctx.pending_indexes.push(idx);
        }
        elem
    }

    /// `$c[label]` is `c = [label: e | ρ] ∨ c = Map e`, decided by `c`'s head
    /// now or at the boundary that owns it (`typecheck/index.rs`).
    fn infer_label_read(&mut self, target: &Ty, label: &str) -> Ty {
        let lbl = Lbl {
            target: target.clone(),
            label: label.to_string(),
            elem: self.ctx.unifier.fresh_ty(),
            span: self.ctx.pos,
        };
        let elem = lbl.elem.clone();
        if matches!(self.ctx.settle_label(&lbl), Settled::Pending) {
            self.ctx.pending_labels.push(lbl);
        }
        elem
    }

    /// A `case` arm's bound payload against what the scrutinee constructs at
    /// its label, forced while pos is on the arm; the final row-unify would
    /// report it with the caret on the whole `case` form.  Returns the
    /// payload type the closed scrutinee row carries at `label`.
    fn case_arm_payload(
        &mut self,
        arm: &JoinArm<'_>,
        payload_ty: &Ty,
        scrut_payload: Option<&Ty>,
    ) -> Ty {
        self.with_span(arm.result_span(), |this| {
            let Some(scrut_payload) = scrut_payload else {
                return payload_ty.clone();
            };
            if this.ctx.unifier.unify_ty(payload_ty, scrut_payload).is_ok() {
                return payload_ty.clone();
            }
            let expected = this.ctx.unifier.apply_ty(scrut_payload);
            let actual = this.ctx.unifier.apply_ty(payload_ty);
            this.ctx.report(
                TypeErrorKind::TyMismatch {
                    expected: Box::new(expected),
                    actual: Box::new(actual),
                },
                Reason::CaseArmPayload,
            );
            this.ctx.unifier.fresh_ty()
        })
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
        let scrut_payloads: std::collections::HashMap<Label, Ty> =
            collect_extends(&scrut_resolved_row).into_iter().collect();

        // Exactly one arm runs, so they share one type: a literal `{ |p| … }`
        // binds its pattern to a fresh payload type, and a thunk in hand is
        // unified.  Source order throughout, so a program's complaints arrive
        // in the order it was written.
        let payloads: Vec<Ty> = arms.iter().map(|_| self.ctx.unifier.fresh_ty()).collect();
        let join_arms: Vec<JoinArm<'_>> = arms
            .iter()
            .zip(&payloads)
            .map(|(arm, payload)| {
                let why = match arm.body.item {
                    Val::Thunk(_) => Reason::CaseArms,
                    _ => Reason::CaseArmHandler,
                };
                JoinArm::new(&arm.body, vec![payload.clone()], why)
            })
            .collect();
        let joined = self.join_arms(&join_arms, &Reason::CaseArms);
        let mut closed = Vec::with_capacity(arms.len());
        for ((arm, join_arm), payload) in arms.iter().zip(&join_arms).zip(&payloads) {
            let label = Label::Case(arm.tag.item.clone());
            let closed_payload =
                self.case_arm_payload(join_arm, payload, scrut_payloads.get(&label));
            closed.push((label, closed_payload));
        }
        let arm_labels: Vec<Label> = closed.iter().map(|(l, _)| l.clone()).collect();
        let closed_scrut = closed.into_iter().rev().fold(Row::Empty, |rest, (l, ty)| {
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
                // Which side a row error names is an artefact of the Rémy
                // rewrite, which swaps labels past each other, so the verdict
                // is read off the two label sets rather than off the error.
                TypeErrorKind::RowExtraField { .. } | TypeErrorKind::RowMissingField { .. } => {
                    coverage_verdict(&scrut_resolved_row, &arm_labels).unwrap_or(kind)
                }
                other => other,
            };
            self.ctx.diagnose(translated);
        }

        joined
    }

    /// The `Rec` rule: bind each name to a self-referential mono
    /// computation type, infer every member in that recursive environment,
    /// unify each against its own type, unbind the self-bindings, and answer
    /// the `index`-th member's type — memoized in `ctx.rec_groups` per
    /// `Arc` identity, so a group is inferred once within a run however many
    /// of its members are projected.
    fn infer_rec(&mut self, group: &Arc<GroupNode>, index: usize) -> CompTy {
        let key = Arc::as_ptr(group).cast::<()>();
        if let Some(betas) = self.ctx.rec_groups.get(&key) {
            return betas[index].clone();
        }
        let members = group.shape();

        let betas: Vec<CompTy> = members
            .iter()
            .map(|_| self.ctx.unifier.fresh_comp_ty())
            .collect();

        for ((name, _), beta) in members.iter().zip(betas.iter()) {
            self.env.bind(
                name.to_string(),
                Scheme::mono(Ty::Thunk(Box::new(beta.clone()))),
            );
        }
        for ((_, member), beta) in members.iter().zip(betas.iter()) {
            let member_ty = self.infer_comp(member);
            self.ctx.unify_comp_ty(&member_ty, beta, Reason::LetRecSelf);
        }
        for (name, _) in members {
            self.env.unbind(name);
        }
        self.ctx.rec_groups.insert(key, betas.clone());
        betas[index].clone()
    }

    /// Note, for the `()` note, that `pattern` binds what a call returned.
    fn note_called(&mut self, pattern: &IrPattern, rhs: &Comp) {
        if let IrPattern::Name(name) = pattern
            && !is_gensym(name)
            && let Some(callee) = Self::called_head(rhs)
        {
            self.env.note_called(name.to_string(), callee);
        }
    }

    /// The head of the call whose value a right-hand side ends in: an `App`, a
    /// `Force`, or a pipeline ending in one.
    fn called_head(rhs: &Comp) -> Option<String> {
        match &rhs.item {
            CompKind::Bind { rest, .. } => Self::called_head(rest),
            CompKind::Pipeline { stages, .. } => stages.last().and_then(|s| Self::called_head(s)),
            CompKind::Force(Val::Variable(name)) => Some(name.to_string()),
            CompKind::App { head, .. } => Self::called_head(head),
            CompKind::Exec(exec) => Some(exec.head.name().written().into_owned()),
            _ => None,
        }
    }

    /// A read of a name bound to what a call returned, when that is `()`.
    fn note_unit_read(&mut self, name: &str, ty: &Ty) {
        if let Some(callee) = self.env.lookup_called(name)
            && matches!(self.ctx.unifier.resolve_ty(ty), Ty::Unit)
            && let Some(pos) = self.ctx.pos
        {
            let call = UnitCall {
                name: name.to_string(),
                callee: callee.to_string(),
            };
            self.ctx.unit_reads.push((pos, call));
        }
    }

    /// A `let`'s right-hand side, inferred as the holder of the indexes it reads.
    fn infer_held(&mut self, pattern: &IrPattern, rhs: &Comp) -> CompTy {
        let mut names = Vec::new();
        collect_pattern_names(pattern, &mut names);
        let outer = self.ctx.holder;
        if names.iter().any(|name| !is_gensym(name)) {
            self.ctx.holder = rhs.span.or(outer);
        }
        let cty = self.infer_comp(rhs);
        self.ctx.holder = outer;
        cty
    }

    pub(super) fn infer_comp(&mut self, comp: &Comp) -> CompTy {
        if let Some(span) = comp.span {
            self.ctx.pos = Some(span);
        }

        match &comp.item {
            CompKind::Return(value) => CompTy::pure(self.infer_val(value)),
            CompKind::Assemble(assembly) => CompTy::pure(self.infer_assembly(assembly)),
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
            // `Bind` on `Wildcard` is a discarded statement — never
            // generalised, and `rest` inherits whatever handler scope an
            // `alias`/`unalias` opened.
            CompKind::Bind {
                comp: inner,
                pattern,
                rest,
            } => {
                match pattern.as_ref() {
                    IrPattern::Wildcard => {
                        self.infer_statement(inner);
                    }
                    pattern => self.infer_let(pattern, inner),
                }
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
                    self.ctx.diagnose(kind);
                    // Check the args anyway, then hand the enclosing pipeline
                    // or chain a coherent fresh result.
                    for slot in args.iter().map(ValListElem::slot) {
                        self.with_span(slot.span, |this| {
                            let _ = this.infer_val(&slot.item);
                        });
                    }
                    return self.ctx.unifier.fresh_comp_ty();
                }
                self.apply_args(head_ty, args)
            }
            CompKind::Exec(e) => {
                let cty = match &e.head {
                    CommandWord::Name(CommandName::Bare(name)) => {
                        self.exec_comp_ty(comp, name, &e.args)
                    }
                    CommandWord::External(name)
                    | CommandWord::Name(
                        name @ (CommandName::Path(_) | CommandName::TildePath(_)),
                    ) => self.external_exec_comp_ty(&name.written(), &e.args),
                };
                if e.redirects.stdout.is_some() {
                    self.discharge(&cty)
                } else {
                    cty
                }
            }
            CompKind::Pipeline { stages, .. } => self.infer_pipeline(stages),
            CompKind::Binary(op, lhs, rhs) => CompTy::pure(self.infer_binary(*op, lhs, rhs)),
            CompKind::Negate(val) => {
                let ty = self.infer_val(val);
                let number = self.ctx.fresh_kinded(Kind::NUMBER);
                self.ctx.unify_ty(&ty, &number, Reason::Negation);
                CompTy::pure(number)
            }
            CompKind::Not(val) => CompTy::pure(self.infer_not(val)),
            CompKind::Interpolation(parts) => {
                for value in parts {
                    let ty = self.infer_val(value);
                    let part = self.ctx.fresh_kinded(Kind::SCALAR);
                    self.ctx.unify_ty(&ty, &part, Reason::Interpolation);
                }
                CompTy::pure(Ty::String)
            }
            CompKind::Index { target, keys } => self.infer_index(target, keys),
            CompKind::Rec { group, index } => self.infer_rec(group, *index),
            CompKind::Tilde(_) => CompTy::pure(Ty::String),
            CompKind::If { cond, then, else_ } => {
                let cond_ty = self.infer_val(&cond.item);
                // Underline just the cond, not the whole `if … else …` form.
                self.with_span(cond.span, |this| {
                    this.ctx.unify_ty(&cond_ty, &Ty::Bool, Reason::IfCond);
                });
                let arms = [then, else_].map(|arm| JoinArm::new(arm, vec![], Reason::IfBranches));
                self.join_arms(&arms, &Reason::IfBranches)
            }
            CompKind::Case { scrutinee, arms } => self.infer_case(scrutinee, arms),
            CompKind::Within {
                opts,
                handlers,
                body,
            } => self.infer_within(opts, handlers.as_deref(), body),
            CompKind::Grant { caps, body } => self.infer_grant(caps, body),
            CompKind::Try { body, handler } => self.infer_try(body, handler),
            CompKind::Guard { body, cleanup } => self.infer_guard(body, cleanup),
            CompKind::Audit { body } => self.infer_audit(body),
            // Installs fds, not its own type; carries the body as an
            // `Arc<Comp>` rather than a thunk-shaped `Val` like the scope
            // forms above, so infer it directly.  A stdout redirect takes a
            // command's output, leaving a value producer.
            CompKind::Redirect { body, redirects } => {
                let cty = self.infer_comp(body);
                if redirects.stdout.is_some() {
                    self.discharge(&cty)
                } else {
                    cty
                }
            }
            // Inserted by `annotate`'s write-back pass, so it is absent from a
            // freshly elaborated tree but present in every tree re-inferred
            // from a live value — a handler arm vetted at install, a bound
            // lambda's body.  `cap M` runs a command and returns what it wrote.
            CompKind::Capture(body) => {
                let body_ty = self.infer_comp(body);
                let shape = CompTy::Return(self.ctx.unifier.fresh_grade(), Box::new(Ty::Unit));
                self.ctx.unify_comp_ty(&body_ty, &shape, Reason::Capture);
                CompTy::pure(Ty::Bytes)
            }
            // The reading half of the same boundary: bytes in, text out.  The
            // kernel's `decode` takes a value, so `annotate` reaches it through
            // a bind over `Capture` rather than nesting the two; the operand
            // here is just the bound variable.
            CompKind::Decode(val) => {
                let ty = self.infer_val(val);
                self.ctx.unify_ty(&ty, &Ty::Bytes, Reason::Capture);
                CompTy::pure(Ty::String)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::syntax::parser::parse;
    use crate::typecheck::{Scheme, Ty};
    use crate::{SessionSchemes, elaborator::elaborate, typecheck};
    use std::sync::Arc;

    fn error_codes(src: &str) -> Vec<&'static str> {
        error_codes_seeded(src, Vec::new())
    }

    /// `src` checked against a session holding `bindings`, which the
    /// elaborator is not told of, so a head naming one reaches the checker as
    /// an `Exec`.
    fn error_codes_seeded(
        src: &str,
        bindings: Vec<(String, Option<Arc<Scheme>>)>,
    ) -> Vec<&'static str> {
        let ast = parse(src).unwrap_or_else(|e| panic!("parse error in {src:?}: {e:?}"));
        let comp = elaborate(&ast, std::collections::HashSet::default(), "")
            .unwrap_or_else(|e| panic!("elaborate error in {src:?}: {e:?}"));
        let schemes = SessionSchemes {
            bindings,
            ..SessionSchemes::default()
        };
        typecheck(&comp, schemes, None)
            .err()
            .unwrap_or_default()
            .iter()
            .map(|e| e.kind.code())
            .collect()
    }

    #[test]
    fn an_unbound_variable_is_refused_where_it_is_read() {
        assert_eq!(error_codes("echo $nope"), ["T0071"]);
        assert_eq!(error_codes("let f = { |x| $y }"), ["T0071"]);
        assert_eq!(error_codes("let a = 1\necho $a"), Vec::<&str>::new());
        assert_eq!(
            error_codes("let t = $true; if $t { echo y } else { echo n }"),
            Vec::<&str>::new()
        );
    }

    /// A session binding of a value is no program; one of unknown type — a
    /// host seed var — is taken to be a thunk, by use.
    #[test]
    fn a_session_head_must_be_a_thunk() {
        let bound = |ty: Option<Ty>| {
            let scheme = ty.map(|ty| Arc::new(Scheme::mono(ty)));
            vec![("date".to_string(), scheme)]
        };
        assert_eq!(
            error_codes_seeded("date +%s", bound(Some(Ty::Int))),
            ["T0072"]
        );
        assert_eq!(
            error_codes_seeded("date +%s", bound(None)),
            Vec::<&str>::new()
        );
    }

    /// A static duplicate key is T0022 in a record, a plain map, and a map
    /// with a spread (`Assembly::Map`) alike.
    #[test]
    fn a_map_literal_refuses_a_static_duplicate_key() {
        assert_eq!(error_codes("return [a: 1, a: 2]"), ["T0022"]);
        assert_eq!(error_codes("return [:, a: 1, a: 2]"), ["T0022"]);
        assert_eq!(
            error_codes("let k = 'x'\nreturn [$k: 1, a: 1, a: 2]"),
            ["T0022"]
        );
    }
}
