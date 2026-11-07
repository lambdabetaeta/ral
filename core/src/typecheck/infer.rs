//! Infer types and build pending CBPV nodes, mutually recursive through thunks.

use super::builtins::{BuiltinDiagnostic, Decl, LANGUAGE_CONSTANTS};
use super::contract::catch_all_shape;
use super::env::{HandlerBinding, HandlerOrigin, InferCtx, Pending, TyEnv};
use super::error::{Reason, Standing, StdinFeed, TypeError, TypeErrorKind, UnitCall};
use super::generalize::generalize;
use super::grade::{Bound, JoinArm};
use crate::data::Str;
use crate::ir::{
    Argv, CaseArm, CommandName, CommandWord, Comp, CompKind, DefineSchemes, Exec, FieldsNode,
    GroupNode, Key, MapEntryV, Name, Pattern, Phrase, Prim, ThunkNode, Val, ValListElem,
    is_synthetic,
};
use crate::ir::{ArithOp, BinaryOp, Redirects, StdinSource};
use crate::source::Span;
use crate::source::Spanned;
use crate::source::WithSpan;
use crate::ty::{CompTy, Kind, Label, Producer, RefusedArg, Row, Scheme, Site, Ty};
use std::sync::Arc;

/// Bind captured bytes before value-only decode; both nodes blame the body.
/// A synthetic binder is unobservable and cannot be rebound by user code.
fn captured_string<S>(body: Comp<S>, ctx: &mut InferCtx) -> CompKind<S> {
    let span = body.span;
    let name = ctx.fresh_name("decode");
    CompKind::bind(
        Pattern::Name(name.clone()),
        Spanned::with_span(span, CompKind::Capture(Arc::new(body))),
        Spanned::with_span(span, CompKind::Prim(Prim::Decode(Val::Variable(name)))),
    )
}

/// Fresh parameters and the arguments that pass them on.
fn eta<S>(ctx: &mut InferCtx, n: usize) -> (Vec<Name>, Vec<Spanned<Val<S>>>) {
    let params: Vec<Name> = (0..n).map(|_| ctx.fresh_name("eta")).collect();
    let args = params
        .iter()
        .map(|p| Spanned::synthetic(Val::Variable(p.clone())))
        .collect();
    (params, args)
}

/// Apply arguments, leaving an argumentless head alone.
fn applied<S>(span: Option<Span>, head: Comp<S>, args: Vec<Spanned<Val<S>>>) -> Comp<S> {
    if args.is_empty() {
        head
    } else {
        Spanned::with_span(
            span,
            CompKind::App {
                head: Arc::new(head),
                args: args.into(),
            },
        )
    }
}

fn exec_node<S>(
    head: &CommandWord,
    argv: Argv<S>,
    redirects: Redirects<Val<S>>,
    site: Option<S>,
) -> CompKind<S> {
    CompKind::Exec(Exec {
        head: head.clone(),
        argv,
        redirects,
        site,
    })
}

/// Enclose the body only when redirects are present.
fn redirected<S>(body: Comp<S>, redirects: Redirects<Val<S>>) -> CompKind<S> {
    if redirects.is_empty() {
        body.item
    } else {
        CompKind::Redirect {
            body: Arc::new(body),
            redirects,
        }
    }
}

/// Thunk the nested lambdas over `params`.
fn lambda<S>(span: Option<Span>, params: Vec<Name>, body: CompKind<S>) -> Val<S> {
    Val::thunk(Arc::new(lambda_comp(span, params, body)))
}

/// Nest lambdas in parameter order.
fn lambda_comp<S>(span: Option<Span>, params: Vec<Name>, body: CompKind<S>) -> Comp<S> {
    params
        .into_iter()
        .rev()
        .fold(Spanned::with_span(span, body), |body, param| {
            Spanned::with_span(
                span,
                CompKind::Lam {
                    param,
                    body: Arc::new(body),
                },
            )
        })
}

/// A boundary value holds a saturated call carrying its site.
fn boundary_block<S>(
    name: &str,
    arity: usize,
    site: S,
    span: Option<Span>,
    ctx: &mut InferCtx,
) -> Val<S> {
    let (params, args) = eta(ctx, arity);
    let call = exec_node(
        &CommandWord::Name(CommandName::Bare(name.into())),
        args.into_iter().map(ValListElem::Single).collect(),
        Redirects::default(),
        Some(site),
    );
    lambda(span, params, call)
}

/// Which slot an arm fills.  The slot, never the arm's spelling, fixes its
/// parameters (SPEC §17.7).
#[derive(Clone, Copy)]
pub(super) enum Arm<'a> {
    /// `handlers: [name: …]` or `alias name …`: the argv.
    Named(&'a str),
    /// `handler: …`: the name, then the argv.
    CatchAll,
}

/// An arm refused, ready to raise, beside the type it is installed at anyway.
pub(super) type ArmVerdict = Result<CompTy, (CompTy, Box<TypeError>)>;

/// What a bare command head resolves to, by the lookup order the checker and
/// the runtime share: binding, value builtin, handler, external.
enum HeadClass {
    /// A lexical or session binding, called at its scheme.
    Binding(Arc<Scheme>),
    /// A value row of the builtin table, applied at its scheme.
    Value(Decl),
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
    let produced: Vec<&Label> = scrutinee.fields().map(|(l, _)| l).collect();
    let missing: Vec<String> = produced
        .iter()
        .copied()
        .filter(|l| !arms.contains(l))
        .map(ToString::to_string)
        .collect();
    let extra: Vec<String> = if scrutinee.is_open() {
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

/// The name a program wrote, not one the compiler bound.
fn written(v: &Val) -> Option<&Name> {
    match v {
        Val::Variable(x) if !is_synthetic(x) => Some(x),
        _ => None,
    }
}

/// An index key as written between the brackets.
fn spell_key(key: &Key) -> String {
    match key {
        Key::Label(label) => label.to_string(),
        Key::Position(n) => n.to_string(),
        Key::Data(v) => written(v).map_or_else(|| "…".into(), |x| format!("${x}")),
    }
}

/// The redirect binding standard input at a stage's own root, if it has one.
///
/// The root is where a feed answers the stage's reads for its whole run: an
/// [`Exec`](CompKind::Exec) carries its redirects, anything else
/// wears them as a [`CompKind::Redirect`] frame; the spine walk skips
/// the binders elaboration hoists out of a redirect's own target
/// (`b < $[locate f]`), whose innermost continuation is the stage as written.
/// A read redirected deeper answers one command's reads and no others, which
/// is that command's business alone, so this walk never sees it.
fn stage_root_stdin_feed(stage: &Comp) -> Option<StdinFeed> {
    let mut stage = stage;
    while let CompKind::Bind { rest, .. } | CompKind::Seq { rest, .. } = &stage.item {
        stage = rest;
    }
    let redirects = match &stage.item {
        CompKind::Exec(exec) => &exec.redirects,
        CompKind::Redirect { redirects, .. } => redirects,
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
    Alias {
        head: &'a CommandWord,
        name: Spanned<&'a Str>,
        block: &'a Spanned<Val>,
    },
    Unalias {
        head: &'a CommandWord,
        name: Spanned<&'a Str>,
    },
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
        if exec
            .argv
            .iter()
            .any(|slot| matches!(slot, ValListElem::Spread(_)))
        {
            return Err(malformed("spread arguments are not allowed here"));
        }
        match (head, &exec.argv[..]) {
            (
                "alias",
                [
                    ValListElem::Single(Spanned {
                        item: Val::String(name),
                        span,
                    }),
                    ValListElem::Single(block),
                ],
            ) if matches!(block.item, Val::Thunk(_)) => Ok(Some(Self::Alias {
                head: &exec.head,
                name: Spanned::with_span(*span, name),
                block,
            })),
            ("alias", _) => Err(malformed("expected `alias name { body }`")),
            (
                _,
                [
                    ValListElem::Single(Spanned {
                        item: Val::String(name),
                        span,
                    }),
                ],
            ) => Ok(Some(Self::Unalias {
                head: &exec.head,
                name: Spanned::with_span(*span, name),
            })),
            _ => Err(malformed("expected `unalias name`")),
        }
    }
}

/// Build phrases with unsettled schemes, returning the original tail's type.
pub(super) fn infer_toplevel(
    ctx: &mut InferCtx,
    env: &mut TyEnv,
    phrases: &[Spanned<Phrase<()>>],
) -> (Vec<Spanned<Phrase<DefineSchemes, Pending>>>, Option<CompTy>) {
    Inferencer { ctx, env }.infer_phrases(phrases)
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

impl Inferencer<'_> {
    /// Bind each returned thunk before applying the next argument segment.
    fn split<S>(
        &mut self,
        span: Option<Span>,
        splits: &[usize],
        mut args: Vec<Spanned<Val<S>>>,
        first: impl FnOnce(Vec<Spanned<Val<S>>>) -> Comp<S>,
    ) -> CompKind<S> {
        let mut later: Vec<_> = splits.iter().rev().map(|&k| args.split_off(k)).collect();
        later.reverse();
        let mut call = first(args);
        let mut binds = Vec::with_capacity(later.len());
        for segment in later {
            let thunk = self.ctx.fresh_name("split");
            let head = Spanned::with_span(span, CompKind::Force(Val::Variable(thunk.clone())));
            binds.push((thunk, call));
            call = applied(span, head, segment);
        }
        binds
            .into_iter()
            .rev()
            .fold(call, |rest, (thunk, rhs)| {
                Spanned::with_span(span, CompKind::bind(Pattern::Name(thunk), rhs, rest))
            })
            .item
    }

    pub(super) fn with_scope<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved_pos = self.ctx.pos;
        self.env.push();
        let out = f(self);
        self.env.pop();
        self.ctx.pos = saved_pos;
        out
    }

    fn bind_pattern(&mut self, pat: &Pattern, ty: &Ty) {
        let mut bindings = Vec::new();
        self.collect_pattern(pat, ty, &mut bindings);
        let schemes: Vec<_> = bindings
            .into_iter()
            .map(|(name, ty)| (name, generalize(&self.ctx.unifier, self.env, &ty)))
            .collect();
        for (name, scheme) in schemes {
            self.env.bind(name.to_string(), scheme);
        }
    }

    fn collect_pattern<'p>(
        &mut self,
        pat: &'p Pattern,
        ty: &Ty,
        bindings: &mut Vec<(&'p Name, Ty)>,
    ) {
        match pat {
            Pattern::Wildcard => {}
            Pattern::Name(name) => bindings.push((name, ty.clone())),
            Pattern::List { elems, rest } => {
                let elem = self.ctx.unifier.fresh_ty();
                self.ctx
                    .unify_ty(ty, &Ty::list(elem.clone()), Reason::ListPattern);
                for elem_pat in elems {
                    self.collect_pattern(elem_pat, &elem, bindings);
                }
                if let Some(rest_name) = rest {
                    bindings.push((rest_name, Ty::list(elem)));
                }
            }
            Pattern::Record(entries) => {
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
                    self.collect_pattern(&entry.pattern, field_ty, bindings);
                }
            }
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
    pub(super) fn force_return_shape(&mut self, cty: &CompTy, why: Reason) -> Producer {
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

    /// The tail whose type is the whole `Bind`/`Seq` chain's type.
    fn discard_tail(comp: &Comp) -> &Comp {
        let mut comp = comp;
        while let CompKind::Bind { rest, .. } | CompKind::Seq { rest, .. } = &comp.item {
            comp = rest;
        }
        comp
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
        let got = exec.argv.arity().unwrap_or(0);
        Some((name.to_string(), entry.fixed_arity(), got))
    }

    /// The span of the expression a computation's result comes from: its tail,
    /// through lambda bodies and each `Bind`/`Seq` continuation.
    pub(super) fn result_span(comp: &Comp) -> Option<Span> {
        let mut comp = comp;
        while let CompKind::Bind { rest, .. } | CompKind::Seq { rest, .. } = &comp.item {
            comp = rest;
        }
        match &comp.item {
            CompKind::Lam { body, .. } => Self::result_span(body),
            _ => comp.span,
        }
    }

    /// Record each returned thunk's force before argument `at`.
    fn autoderef_thunk_return(
        &mut self,
        mut cty: CompTy,
        at: usize,
        splits: &mut Vec<usize>,
    ) -> CompTy {
        loop {
            cty = match self.ctx.unifier.resolve_comp_ty(&cty) {
                CompTy::Return(_, ty) => match self.ctx.unifier.resolve_ty(&ty) {
                    Ty::Thunk(inner) => *inner,
                    // A still-free result that meets an argument must be a
                    // thunk, which the split forces.
                    Ty::Var(_) => {
                        let inner = self.ctx.unifier.fresh_comp_ty();
                        self.ctx.unify_ty(
                            &ty,
                            &Ty::Thunk(Box::new(inner.clone())),
                            Reason::AutoderefHead,
                        );
                        inner
                    }
                    _ => return cty,
                },
                _ => return cty,
            };
            splits.push(at);
        }
    }

    /// Refuse a spread that reached an application, blaming the spread itself
    /// rather than the whole call.  Only the first is named: the rest are the
    /// same mistake, and one fix answers them all.
    pub(super) fn refuse_spread(&mut self, argv: &Argv, head: super::error::SpreadHead) {
        let Some(span) = argv.iter().find_map(|e| match e {
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

    /// Apply arguments, returning their nodes and force positions.
    pub(super) fn apply_args<'v>(
        &mut self,
        cty: CompTy,
        args: impl IntoIterator<Item = &'v Spanned<Val>>,
    ) -> (CompTy, Vec<Spanned<Val<Pending>>>, Vec<usize>) {
        self.apply_args_capped(cty, args, usize::MAX)
    }

    /// Infer surplus arguments for their own errors, without cascading arity mismatches.
    fn apply_args_capped<'v>(
        &mut self,
        mut cty: CompTy,
        args: impl IntoIterator<Item = &'v Spanned<Val>>,
        cap: usize,
    ) -> (CompTy, Vec<Spanned<Val<Pending>>>, Vec<usize>) {
        // Later arguments fix `map`'s element type before its literal body reads it.
        // Rebuild deferred bodies into their original argument slots.
        let mut deferred = Vec::new();
        let mut splits = Vec::new();
        let mut rebuilt = Vec::new();
        for (i, arg) in args.into_iter().enumerate() {
            if i >= cap {
                let _ = self.infer_val(&arg.item, arg.span);
                continue;
            }
            cty = self.autoderef_thunk_return(cty, i, &mut splits);
            cty = self.with_span(arg.span, |this| {
                if let Val::Thunk(body) = &arg.item {
                    let wanted = this.ctx.unifier.fresh_comp_ty();
                    deferred.push((i, arg.span, body, wanted.clone()));
                    rebuilt.push(None);
                    this.apply_to(&cty, Ty::Thunk(Box::new(wanted)))
                } else {
                    let (ty, value) = this.infer_val(&arg.item, arg.span);
                    rebuilt.push(Some(Spanned::with_span(arg.span, value)));
                    this.apply_to(&cty, ty)
                }
            });
        }
        for (i, span, body, wanted) in deferred {
            rebuilt[i] = Some(self.demand_block(span, body, &wanted));
        }
        (
            cty,
            rebuilt
                .into_iter()
                .map(|v| v.expect("argument inferred"))
                .collect(),
            splits,
        )
    }

    fn demand_block(
        &mut self,
        span: Option<Span>,
        body: &ThunkNode,
        wanted: &CompTy,
    ) -> Spanned<Val<Pending>> {
        self.with_span(span, |this| {
            let (given, body) = this.with_scope(|this| this.check_comp(body.shape(), wanted));
            this.ctx.unify_comp_ty(wanted, &given, Reason::Argument);
            Spanned::with_span(span, Val::thunk(Arc::new(body)))
        })
    }

    /// `callee` applied to an argument of type `ty`: the residual computation.
    fn apply_to(&mut self, callee: &CompTy, ty: Ty) -> CompTy {
        let result = self.ctx.unifier.fresh_comp_ty();
        let expected = CompTy::Fun(Box::new(ty), Box::new(result.clone()));
        self.ctx.unify_comp_ty(callee, &expected, Reason::Argument);
        result
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
        entry: &Decl,
        name: &str,
        argv: &Argv,
    ) -> (CompTy, Vec<Spanned<Val<Pending>>>, Vec<usize>) {
        let fixed_arity = entry.fixed_arity();

        // There is no positional reading exactly when the call writes a
        // `...`, and a builtin takes its arguments by application, which has
        // no argv to spread into.
        let Some(positional) = argv.positional() else {
            self.infer_refused_args(argv.iter().map(ValListElem::slot));
            self.refuse_spread(
                argv,
                super::error::SpreadHead::Builtin {
                    name: name.into(),
                    arity: fixed_arity,
                },
            );
            // Nothing was applied and nothing is residual: the refusal is the
            // whole story of this call, so its type is the saturated result.
            return (self.saturated_result(name, entry), Vec::new(), Vec::new());
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

        let scheme = entry.scheme(&mut self.ctx.unifier);
        let head_cty = self.instantiate_comp(name, &scheme);
        self.apply_args_capped(head_cty, positional, fixed_arity)
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
    fn saturated_result(&mut self, name: &str, entry: &Decl) -> CompTy {
        let scheme = entry.scheme(&mut self.ctx.unifier);
        let cty = self.instantiate_comp(name, &scheme);
        self.peel_curry_spine(&cty)
    }

    /// A forced head's value type when it is concretely no thunk — neither a
    /// `Thunk` nor a variable that could still become one.  Lets the `App` rule
    /// name `'foo' bar baz` before the general unifier mismatch fires.
    fn non_thunk_ty(&self, ty: &Ty) -> Option<Ty> {
        match self.ctx.unifier.resolve_ty(ty) {
            Ty::Thunk(_) | Ty::Var(_) => None,
            concrete => Some(concrete),
        }
    }

    /// The `Force` rule on `value`, of type `ty`: the computation it
    /// suspends, a thunk demanded under `why`.
    fn forced(&mut self, value: &Val, ty: &Ty, why: Reason) -> CompTy {
        if let Some(name) = written(value) {
            return self.thunk_body(name, ty, why);
        }
        let cty = self.ctx.unifier.fresh_comp_ty();
        self.ctx
            .unify_ty(ty, &Ty::Thunk(Box::new(cty.clone())), why);
        cty
    }

    /// `(arm)`: `block`, written or in hand, takes the parameters its slot
    /// hands it and produces what the head it stands in for produces.  Its
    /// type is what is checked, never its spelling, so `{ forward 'git' }` is
    /// as unary as `{ |args| … }`.  A refusal comes back ready to raise, with
    /// the type the arm is installed at anyway: the run-time door renders it
    /// in its own words.  A join's arm meets its join in place instead
    /// (`check_arm`).
    pub(super) fn arm(
        &mut self,
        block: &Val,
        arm: Arm<'_>,
        at: Option<Span>,
    ) -> (ArmVerdict, Val<Pending>) {
        let producer = CompTy::Return(
            self.ctx.unifier.fresh_grade(),
            Box::new(self.ctx.unifier.fresh_ty()),
        );
        let (wanted, why) = match arm {
            Arm::Named(_) => (CompTy::arrows([Ty::argv()], producer), Reason::AliasParam),
            Arm::CatchAll => (catch_all_shape(producer), Reason::CatchAllParams),
        };
        let (met, node) = match block {
            Val::Thunk(comp) => {
                let (got, node) = self.with_scope(|this| this.check_comp(comp.shape(), &wanted));
                (
                    self.ctx
                        .try_unify_comp_ty(&wanted, &got)
                        .map_err(|kind| (kind, why)),
                    Val::thunk(Arc::new(node)),
                )
            }
            // A value in hand is a block first, then one of the slot's type.
            held => {
                let (ty, node) = self.infer_val(held, at);
                let got = self.ctx.unifier.fresh_comp_ty();
                let met = match self
                    .ctx
                    .try_unify_ty(&ty, &Ty::Thunk(Box::new(got.clone())))
                {
                    Ok(()) => self
                        .ctx
                        .try_unify_comp_ty(&wanted, &got)
                        .map_err(|kind| (kind, why)),
                    Err(kind) => Err((kind, Reason::HandlerArm)),
                };
                (met, node)
            }
        };
        let (head, standing) = self.stands_for(arm);
        let stands = self.arm_stands_in(&wanted, &head, standing);
        let verdict = match met {
            Ok(()) => stands,
            Err((kind, why)) => {
                let installed = stands.unwrap_or_else(|(cty, _)| cty);
                Err((installed, self.refusal(kind, why)))
            }
        };
        (verdict, node)
    }

    /// [`Self::arm`] at the checker, a refusal raised: the type the arm is
    /// installed at.
    pub(super) fn checked_arm(
        &mut self,
        block: &Val,
        arm: Arm<'_>,
        at: Option<Span>,
    ) -> (CompTy, Val<Pending>) {
        let (verdict, node) = self.arm(block, arm, at);
        let cty = verdict.unwrap_or_else(|(cty, error)| {
            self.ctx.raise(*error);
            cty
        });
        (cty, node)
    }

    pub(super) fn install_arm(
        &mut self,
        block: &Val,
        name: &str,
        at: Option<Span>,
    ) -> (Scheme, Val<Pending>) {
        let (cty, node) = self.checked_arm(block, Arm::Named(name), at);
        let thunk_ty = Ty::Thunk(Box::new(cty));
        (generalize(&self.ctx.unifier, self.env, &thunk_ty), node)
    }

    /// What the head an arm stands in for is: under a name, the base frame or
    /// arm already in force there, else a command — an external, or a value
    /// row's name, whose arm is dead because the table is searched first.  The
    /// catch-all stands in for every command.
    fn stands_for(&mut self, arm: Arm<'_>) -> (CompTy, Standing) {
        let Arm::Named(name) = arm else {
            return (command_head(), Standing::EveryCommand);
        };
        match self.env.lookup_handler(name).cloned() {
            Some(handler) => (
                self.instantiate_comp(name, &handler.scheme),
                Standing::Own(name.to_string()),
            ),
            None => (command_head(), Standing::Command(name.to_string())),
        }
    }

    /// Unify the arm and head producers.
    fn arm_stands_in(&mut self, arm: &CompTy, head: &CompTy, standing: Standing) -> ArmVerdict {
        let wanted = self.producer_of(head);
        let found = self.producer_of(arm);
        let verdict = self.ctx.try_unify_comp_ty(&wanted, &found);
        // Installed at the head's producer either way, so a refused arm is
        // refused once, not again at every call.
        let wanted = self.ctx.unifier.apply_comp_ty(&wanted);
        let installed = self.map_producer(arm, |_| wanted);
        match verdict {
            Ok(()) => Ok(installed),
            Err(kind) => Err((installed, self.refusal(kind, Reason::StandsIn(standing)))),
        }
    }

    /// A refusal of `kind` under `why`, at the span in force, ready to raise.
    fn refusal(&self, kind: TypeErrorKind, why: Reason) -> Box<TypeError> {
        Box::new(TypeError {
            pos: self.ctx.pos,
            kind,
            reason: Some(why),
            weak: None,
            unit: None,
        })
    }

    /// What `cty` produces once every parameter is supplied, forced to
    /// `Return` shape.
    fn producer_of(&mut self, cty: &CompTy) -> CompTy {
        let tail = self.spine(cty).tail;
        self.force_return_shape(&tail, Reason::ReturnShape).into()
    }

    /// Instantiate the scheme head `name` is bound at and strip the outer
    /// `Thunk` it carries.
    fn instantiate_comp(&mut self, name: &str, scheme: &Scheme) -> CompTy {
        let ty = self.ctx.instantiate(scheme);
        self.thunk_body(name, &ty, Reason::AutoderefHead)
    }

    /// The computation `ty`, the type `name` is bound at, suspends.  A type
    /// still a variable is a thunk by use; `name` bound to anything else is
    /// no program.
    fn thunk_body(&mut self, name: &str, ty: &Ty, reason: Reason) -> CompTy {
        match self.ctx.unifier.resolve_ty(ty) {
            Ty::Thunk(body) => *body,
            Ty::Var(_) => {
                let body = self.ctx.unifier.fresh_comp_ty();
                self.ctx
                    .unify_ty(ty, &Ty::Thunk(Box::new(body.clone())), reason);
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
    /// site's argv, which every arm takes as its one parameter
    /// ([`Self::arm`]).  The argv rule's contribution is its walk and its
    /// refusals: its type is always [`Ty::argv`].
    fn apply_alias_arm(
        &mut self,
        name: &str,
        scheme: &Scheme,
        argv: &Argv,
    ) -> (CompTy, Argv<Pending>) {
        let cty = self.instantiate_comp(name, scheme);
        let argv = self.argv_ty(argv, ArgvBoundary::InShell(name));
        let CompTy::Fun(_, body) = self.ctx.unifier.resolve_comp_ty(&cty) else {
            unreachable!(
                "every arm takes the argv: `Inferencer::arm` installs it so, as every base frame is"
            )
        };
        (*body, argv)
    }

    /// `comp` against a type already known: a block whose parameter the
    /// expectation names binds *that* type rather than a fresh one, so the
    /// body reads a parameter its own call site determined.  Anything else has
    /// nothing to push inwards and is inferred as ever, the caller unifying.
    pub(super) fn check_comp(&mut self, comp: &Comp, expected: &CompTy) -> (CompTy, Comp<Pending>) {
        match (&comp.item, self.ctx.unifier.resolve_comp_ty(expected)) {
            (CompKind::Lam { param, body }, CompTy::Fun(param_ty, result)) => {
                let (body_ty, body) = self.with_scope(|this| {
                    this.env
                        .bind(param.to_string(), Scheme::mono(*param_ty.clone()));
                    this.check_comp(body, &result)
                });
                (
                    CompTy::Fun(param_ty, Box::new(body_ty)),
                    Spanned::with_span(
                        comp.span,
                        CompKind::Lam {
                            param: param.clone(),
                            body: Arc::new(body),
                        },
                    ),
                )
            }
            _ => self.infer_comp(comp),
        }
    }

    fn infer_not(&mut self, val: &Val) -> (Ty, Prim<Pending>) {
        let (ty, val) = self.infer_val(val, None);
        self.ctx.unify_ty(&ty, &Ty::Bool, Reason::NotOperand);
        (Ty::Bool, Prim::Not(val))
    }

    fn infer_binary(&mut self, op: BinaryOp, lhs: &Val, rhs: &Val) -> (Ty, Prim<Pending>) {
        let (lhs_ty, lhs) = self.infer_val(lhs, None);
        let (rhs_ty, rhs) = self.infer_val(rhs, None);
        let why = Reason::BinaryOperands(op);
        let operand = match op {
            BinaryOp::Arith(_) => self.ctx.fresh_kinded(Kind::NUMBER),
            BinaryOp::Compare(_) => self.ctx.fresh_kinded(Kind::COMPARABLE),
            BinaryOp::Eq(_) => self.ctx.fresh_kinded(Kind::DATA),
        };
        self.ctx.unify_ty(&operand, &lhs_ty, why.clone());
        self.ctx.unify_ty(&operand, &rhs_ty, why.clone());
        if op == BinaryOp::Arith(ArithOp::Mod) {
            self.ctx.unify_ty(&operand, &Ty::Int, why);
        }
        let ty = match op {
            BinaryOp::Eq(_) | BinaryOp::Compare(_) => Ty::Bool,
            BinaryOp::Arith(_) => operand,
        };
        (ty, Prim::Binary(op, lhs, rhs))
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
    fn exec_comp_ty(
        &mut self,
        comp: &Comp,
        name: &str,
        exec: &Exec,
        redirects: Redirects<Val<Pending>>,
    ) -> (CompTy, CompKind<Pending>) {
        let argv = &exec.argv;
        match self.head_class(name) {
            HeadClass::Binding(scheme) => {
                let ty = self.ctx.instantiate(&scheme);
                let imported = self.ctx.note_residual_use(name, &scheme, &ty);
                let body = self.thunk_body(name, &ty, Reason::AutoderefHead);
                let Some(args) = argv.positional() else {
                    self.infer_refused_args(argv.iter().map(ValListElem::slot));
                    self.refuse_spread(argv, super::error::SpreadHead::Applied);
                    return (self.peel_curry_spine(&body), CompKind::Return(Val::Unit));
                };
                let (cty, args, splits) = self.apply_args(body, args);
                let call = if imported {
                    let head = Spanned::with_span(
                        comp.span,
                        CompKind::Force(Val::Protected {
                            value: Box::new(Val::Variable(name.into())),
                            site: Pending::Open(ty),
                        }),
                    );
                    self.split(comp.span, &splits, args, |args| {
                        applied(comp.span, head, args)
                    })
                } else {
                    return (
                        cty,
                        self.exec_call(comp.span, &exec.head, args, &splits, redirects, None),
                    );
                };
                (
                    cty,
                    redirected(Spanned::with_span(comp.span, call), redirects),
                )
            }
            HeadClass::Value(entry) => {
                let (cty, args, splits) = self.apply_builtin(&entry, name, argv);
                let boundary = self.note_boundary_call(&entry, argv, &cty);
                let (site, missing) = match boundary {
                    Some((ty, missing)) => (Some(Pending::Open(ty)), missing),
                    None => (None, 0),
                };
                if missing > 0 {
                    let (params, extra) = eta(self.ctx, missing);
                    let mut argv: Argv<Pending> =
                        args.into_iter().map(ValListElem::Single).collect();
                    argv.extend(extra.into_iter().map(ValListElem::Single));
                    let call = exec_node(&exec.head, argv, redirects, site);
                    return (cty, lambda_comp(comp.span, params, call).item);
                }
                (
                    cty,
                    self.exec_call(comp.span, &exec.head, args, &splits, redirects, site),
                )
            }
            HeadClass::Arm(handler) => {
                let (cty, argv) = self.apply_alias_arm(name, &handler.scheme, argv);
                (cty, exec_node(&exec.head, argv, redirects, None))
            }
            // Prelude functions arrive as `App` on bindings, not bare `Exec` heads.
            HeadClass::External => {
                let (cty, argv) = self.external_exec_comp_ty(name, argv);
                (cty, exec_node(&exec.head, argv, redirects, None))
            }
        }
    }

    /// Fuse redirects into an unsplit call; otherwise enclose the whole split.
    fn exec_call(
        &mut self,
        span: Option<Span>,
        head: &CommandWord,
        args: Vec<Spanned<Val<Pending>>>,
        splits: &[usize],
        redirects: Redirects<Val<Pending>>,
        site: Option<Pending>,
    ) -> CompKind<Pending> {
        if splits.is_empty() {
            exec_node(
                head,
                args.into_iter().map(ValListElem::Single).collect(),
                redirects,
                site,
            )
        } else {
            let call = self.split(span, splits, args, |args| {
                Spanned::with_span(
                    span,
                    exec_node(
                        head,
                        args.into_iter().map(ValListElem::Single).collect(),
                        Redirects::default(),
                        site,
                    ),
                )
            });
            redirected(Spanned::with_span(span, call), redirects)
        }
    }

    fn head_class(&self, name: &str) -> HeadClass {
        if let Some(scheme) = self.env.lookup_binding(name) {
            return HeadClass::Binding(Arc::clone(scheme));
        }
        if let Some(entry) = self.env.builtins.value(name) {
            return HeadClass::Value(entry.clone());
        }
        if let Some(handler) = self.env.lookup_handler(name) {
            return HeadClass::Arm(handler.clone());
        }
        HeadClass::External
    }

    /// A call of a boundary row: its result is weak, and the node gets a site.
    fn note_boundary_call(
        &mut self,
        entry: &Decl,
        argv: &Argv,
        cty: &CompTy,
    ) -> Option<(Ty, usize)> {
        if !entry.is_boundary() {
            return None;
        }
        let producer = self.spine(cty).producer()?;
        self.ctx.record_boundary(&entry.name, &producer.ty);
        let arity = entry.fixed_arity();
        let given = argv.arity().unwrap_or(arity);
        Some((producer.ty, arity.saturating_sub(given)))
    }

    /// An external takes an argv and is a command: its value is its output.
    ///
    /// Nothing here declares a parameter for that argv to meet, so the rule's
    /// contribution is its walk and its refusals rather than its type.
    fn external_exec_comp_ty(&mut self, shown: &str, argv: &Argv) -> (CompTy, Argv<Pending>) {
        let argv = self.argv_ty(argv, ArgvBoundary::Exec(shown));
        (CompTy::command(), argv)
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
    fn argv_ty(&mut self, argv: &Argv, boundary: ArgvBoundary<'_>) -> Argv<Pending> {
        argv.iter()
            .map(|entry| {
                self.with_span(entry.slot().span, |this| match entry {
                    ValListElem::Single(arg) => {
                        let (ty, value) = this.infer_val(&arg.item, arg.span);
                        this.gate_arg(&ty, boundary);
                        ValListElem::Single(Spanned::with_span(arg.span, value))
                    }
                    // An empty spread refuses nothing; runtime gates actual elements.
                    ValListElem::Spread(arg) => {
                        let (spread_ty, value) = this.infer_val(&arg.item, arg.span);
                        let elem = this.ctx.unifier.fresh_ty();
                        this.ctx
                            .unify_ty(&spread_ty, &Ty::list(elem), Reason::ListSpread);
                        ValListElem::Spread(Spanned::with_span(arg.span, value))
                    }
                })
            })
            .collect()
    }

    /// Refuse an argv element the boundary has no argument for, one step
    /// before the spawn that would refuse it — which is what makes the promise
    /// that argument-type errors are reported before execution true of an
    /// external's arguments too.
    ///
    /// A concrete type only.  Where the type is still a variable the shape is
    /// genuinely unknown here, and `shell::exec::command::validate_argv` keeps the question:
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
    pub(super) fn infer_refused_args<'v>(
        &mut self,
        args: impl IntoIterator<Item = &'v Spanned<Val>>,
    ) {
        for slot in args {
            self.with_span(slot.span, |this| {
                let _ = this.infer_val(&slot.item, slot.span);
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
        phrases: &[Spanned<Phrase<()>>],
    ) -> (Vec<Spanned<Phrase<DefineSchemes, Pending>>>, Option<CompTy>) {
        let mut schemes = Vec::with_capacity(phrases.len());
        let mut tail = None;
        for phrase in phrases {
            let (names, cty) = self.with_span(phrase.span, |this| this.infer_phrase(&phrase.item));
            schemes.push(Spanned::with_span(phrase.span, names));
            tail = cty;
        }
        (schemes, tail)
    }

    /// One phrase.  Every `Run`'s value is held to the discarded shape, the
    /// tail's included — its own writes are never captured into the report.
    /// Only a `Run` has a value, so only a `Run` hands a `CompTy` back.
    fn infer_phrase(
        &mut self,
        phrase: &Phrase<()>,
    ) -> (Phrase<DefineSchemes, Pending>, Option<CompTy>) {
        match phrase {
            Phrase::Define { pattern, comp, .. } => {
                let comp = self.infer_let(pattern, comp);
                let schemes = pattern
                    .names()
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
                (
                    Phrase::Define {
                        pattern: Arc::clone(pattern),
                        comp,
                        schemes,
                    },
                    None,
                )
            }
            Phrase::Run(comp) => {
                let (cty, comp) = self.infer_statement(comp);
                (Phrase::Run(Arc::new(comp)), Some(cty))
            }
        }
    }

    /// Infer, bind and generalise a `let` right-hand side.
    fn infer_let(&mut self, pattern: &Pattern, rhs: &Comp) -> Arc<Comp<Pending>> {
        let (cty, node) = self.infer_comp(rhs);
        let (bound_ty, bound) = self.rhs_bound_ty(cty);
        let node = match bound {
            Bound::Thunked => {
                Spanned::with_span(rhs.span, CompKind::Return(Val::thunk(Arc::new(node))))
            }
            Bound::Captured => Spanned::with_span(rhs.span, captured_string(node, self.ctx)),
            Bound::Ran => node,
        };
        let concrete = self.ctx.unifier.apply_ty_keeping_weak(&bound_ty);
        self.bind_pattern(pattern, &concrete);
        self.note_called(pattern, rhs);
        Arc::new(node)
    }

    /// A discarded statement: an `alias`/`unalias` binds or unbinds its
    /// handler scheme for what follows, and anything else is inferred and
    /// held to the discarded shape.
    fn infer_statement(&mut self, comp: &Comp) -> (CompTy, Comp<Pending>) {
        let handled = match HandlerStatement::of(comp) {
            Ok(Some(HandlerStatement::Alias { head, name, block })) => {
                let (scheme, arm) = self.install_arm(&block.item, name.item.as_str(), block.span);
                self.env
                    .bind_handler(name.item.to_string(), scheme, HandlerOrigin::Alias);
                let argv = vec![
                    ValListElem::Single(Spanned::with_span(
                        name.span,
                        Val::String(name.item.clone()),
                    )),
                    ValListElem::Single(Spanned::with_span(block.span, arm)),
                ]
                .into();
                Some(exec_node(head, argv, Redirects::default(), None))
            }
            Ok(Some(HandlerStatement::Unalias { head, name })) => {
                self.env.unbind_removable_handler(name.item.as_str());
                let argv = vec![ValListElem::Single(Spanned::with_span(
                    name.span,
                    Val::String(name.item.clone()),
                ))]
                .into();
                Some(exec_node(head, argv, Redirects::default(), None))
            }
            Ok(None) => None,
            Err(kind) => {
                self.ctx.diagnose(kind);
                Some(CompKind::Return(Val::Unit))
            }
        };
        let (cty, node) = if let Some(item) = handled {
            (self.produces(Ty::Unit), Spanned::with_span(comp.span, item))
        } else {
            self.infer_comp(comp)
        };
        self.force_discarded_shape(comp, &cty);
        (cty, node)
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
    /// record and an update ([`Prim::Update`]).
    fn infer_record_field(
        &mut self,
        key: &str,
        value: &Spanned<Val>,
        fields: &mut Vec<(String, Ty)>,
    ) -> Spanned<Val<Pending>> {
        let (duplicate, ty, node) = self.with_span(value.span, |this| {
            let duplicate = this.diagnose_if_duplicate(key, fields.iter().map(|(k, _)| k.as_str()));
            let (ty, node) = this.infer_val(&value.item, value.span);
            (duplicate, ty, node)
        });
        if !duplicate {
            fields.push((key.to_string(), ty));
        }
        Spanned::with_span(value.span, node)
    }

    /// A plain record literal: no spread, so one row, closed.
    fn infer_record_val(
        &mut self,
        entries: &[(Name, Spanned<Val>)],
    ) -> (Ty, Arc<FieldsNode<Pending>>) {
        let mut fields: Vec<(String, Ty)> = Vec::new();
        let nodes = entries
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    self.infer_record_field(key, value, &mut fields),
                )
            })
            .collect();
        (
            Ty::Record(Self::fields_row(fields, Row::Empty)),
            FieldsNode::new(nodes),
        )
    }

    /// Update existing slots, preserving the base's row tail.
    fn infer_update(
        &mut self,
        base: &Spanned<Val>,
        entries: &[(Name, Spanned<Val>)],
    ) -> (Ty, Prim<Pending>) {
        let (base_ty, base_node) =
            self.with_span(base.span, |this| this.infer_val(&base.item, base.span));
        let mut fields = Vec::new();
        let nodes = entries
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    self.infer_record_field(key, value, &mut fields),
                )
            })
            .collect();
        let tail = self.ctx.unifier.fresh_row();
        let probe = fields.iter().rev().fold(tail.clone(), |rest, (key, _)| {
            let old = self.ctx.unifier.fresh_ty();
            Row::Extend(Label::Field(key.clone()), Box::new(old), Box::new(rest))
        });
        let reason = Reason::RecordUpdate {
            base: written(&base.item).map(ToString::to_string),
        };
        self.with_span(base.span, |this| {
            this.ctx.unify_ty(&base_ty, &Ty::Record(probe), reason);
        });
        (
            Ty::Record(Self::fields_row(fields, tail)),
            Prim::Update {
                base: Spanned::with_span(base.span, base_node),
                fields: nodes,
            },
        )
    }

    /// A plain map literal: `Map<elem>`, every key written out — so the
    /// checker refuses a duplicate exactly as a record's (T0022).
    fn infer_map_val(
        &mut self,
        entries: &[(Name, Spanned<Val>)],
    ) -> (Ty, Arc<FieldsNode<Pending>>) {
        let elem = self.ctx.unifier.fresh_ty();
        let mut seen: Vec<Name> = Vec::new();
        let mut nodes = Vec::new();
        for (key, value) in entries {
            let (duplicate, node) = self.with_span(value.span, |this| {
                let duplicate = this.diagnose_if_duplicate(key, seen.iter().map(AsRef::as_ref));
                let (value_ty, node) = this.infer_val(&value.item, value.span);
                this.ctx.unify_ty(&value_ty, &elem, Reason::MapElem);
                (duplicate, node)
            });
            if !duplicate {
                seen.push(key.clone());
            }
            nodes.push((key.clone(), Spanned::with_span(value.span, node)));
        }
        (Ty::map(elem), FieldsNode::new(nodes.into()))
    }

    /// One element type; repeated computed keys are refused at run time.
    fn infer_map(&mut self, entries: &[MapEntryV]) -> (Ty, Prim<Pending>) {
        let elem = self.ctx.unifier.fresh_ty();
        let mut seen: Vec<String> = Vec::new();
        let mut nodes = Vec::new();
        for entry in entries {
            match entry {
                MapEntryV::Entry(key, value) => {
                    let (key_ty, key_node) =
                        self.with_span(key.span, |this| this.infer_val(&key.item, key.span));
                    self.with_span(key.span, |this| {
                        this.ctx.unify_ty(&key_ty, &Ty::String, Reason::MapKey);
                        if let Val::String(label) = &key.item {
                            this.diagnose_if_duplicate(
                                label.as_str(),
                                seen.iter().map(String::as_str),
                            );
                            seen.push(label.as_str().to_string());
                        }
                    });
                    let (value_ty, value_node) =
                        self.with_span(value.span, |this| this.infer_val(&value.item, value.span));
                    self.with_span(value.span, |this| {
                        this.ctx.unify_ty(&value_ty, &elem, Reason::MapElem);
                    });
                    nodes.push(MapEntryV::Entry(
                        Spanned::with_span(key.span, key_node),
                        Spanned::with_span(value.span, value_node),
                    ));
                }
                MapEntryV::Spread(value) => {
                    let (spread_ty, value_node) =
                        self.with_span(value.span, |this| this.infer_val(&value.item, value.span));
                    self.with_span(value.span, |this| {
                        this.ctx
                            .unify_ty(&spread_ty, &Ty::map(elem.clone()), Reason::MapSpread);
                    });
                    nodes.push(MapEntryV::Spread(Spanned::with_span(
                        value.span, value_node,
                    )));
                }
            }
        }
        (Ty::map(elem), Prim::Map(nodes.into()))
    }

    /// One list element's contribution to `elem`, the list's shared element
    /// type: a plain value unifies directly, a spread's own element type
    /// does. Shared by a plain list and [`Prim::List`].
    fn infer_list_entry(
        &mut self,
        value: &Spanned<Val>,
        spread: bool,
        elem: &Ty,
    ) -> Spanned<Val<Pending>> {
        self.with_span(value.span, |this| {
            let (ty, node) = this.infer_val(&value.item, value.span);
            let entry_ty = if spread {
                let inner = this.ctx.unifier.fresh_ty();
                this.ctx
                    .unify_ty(&ty, &Ty::list(inner.clone()), Reason::ListSpread);
                inner
            } else {
                ty
            };
            this.ctx.unify_ty(&entry_ty, elem, Reason::ListElem);
            Spanned::with_span(value.span, node)
        })
    }

    /// A spread-bearing list shares one type across all contributed elements.
    fn infer_list(&mut self, elems: &[ValListElem]) -> (Ty, Prim<Pending>) {
        let elem = self.ctx.unifier.fresh_ty();
        let nodes = elems
            .iter()
            .map(|entry| match entry {
                ValListElem::Single(v) => {
                    ValListElem::Single(self.infer_list_entry(v, false, &elem))
                }
                ValListElem::Spread(v) => {
                    ValListElem::Spread(self.infer_list_entry(v, true, &elem))
                }
            })
            .collect();
        (Ty::list(elem), Prim::List(nodes))
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
        let boundary = super::explain::hyphen_boundary(name, |p| {
            self.env.lookup_binding(p).is_some()
                || self.env.builtins.value_names().any(|n| n == p)
                || LANGUAGE_CONSTANTS.iter().any(|&(c, _)| c == p)
        })
        .map(str::to_string);
        self.ctx.diagnose(TypeErrorKind::UnboundVariable {
            name: name.to_string(),
            suggestions,
            boundary,
        });
    }

    /// Boundary values hold a saturated call.
    fn builtin_value(&mut self, entry: &Decl, at: Option<Span>) -> (Ty, Val<Pending>) {
        let scheme = entry.scheme(&mut self.ctx.unifier);
        let ty = self.ctx.instantiate(&scheme);
        if entry.is_boundary()
            && let Ty::Thunk(body) = &ty
            && let Some(producer) = self.spine(body).producer()
        {
            self.ctx.record_boundary(&entry.name, &producer.ty);
            let node = boundary_block(
                &entry.name,
                entry.fixed_arity(),
                Pending::Open(producer.ty),
                at,
                self.ctx,
            );
            return (ty, node);
        }
        (ty, Val::Variable(entry.name.clone().into()))
    }

    /// Infer a value against a fresh instance of its retained contract.
    fn infer_protected_val(&mut self, value: &Val, site: &Arc<Site>) -> (Ty, Val<Pending>) {
        let (have, value) = self.infer_val(value, None);
        let want = site.instantiate(&mut self.ctx.unifier);
        self.ctx.unify_ty(&have, &want, Reason::ProtectContract);
        (
            want,
            Val::Protected {
                value: Box::new(value),
                site: Pending::Kept(Arc::clone(site)),
            },
        )
    }

    /// `at` places boundary blocks; it never changes the diagnostic position.
    pub(super) fn infer_val(&mut self, val: &Val, at: Option<Span>) -> (Ty, Val<Pending>) {
        match val {
            Val::Protected { value, site } => self.infer_protected_val(value, site),
            Val::Unit => (Ty::Unit, Val::Unit),
            Val::String(v) => (Ty::String, Val::String(v.clone())),
            Val::Int(v) => (Ty::Int, Val::Int(*v)),
            Val::Float(v) => (Ty::Float, Val::Float(*v)),
            Val::Bool(v) => (Ty::Bool, Val::Bool(*v)),
            Val::Variable(name) => {
                if crate::syntax::is_control_operator(name) {
                    self.ctx.diagnose(TypeErrorKind::ControlOperatorAsValue {
                        name: name.to_string(),
                    });
                    (self.ctx.unifier.fresh_ty(), Val::Unit)
                } else {
                    match self.env.lookup_binding(name).cloned() {
                        Some(scheme) => {
                            let ty = self.ctx.instantiate(&scheme);
                            let imported = self.ctx.note_residual_use(name, &scheme, &ty);
                            self.note_unit_read(name, &ty);
                            let value = Val::Variable(name.clone());
                            let value = if imported {
                                Val::Protected {
                                    value: Box::new(value),
                                    site: Pending::Imported(ty.clone()),
                                }
                            } else {
                                value
                            };
                            (ty, value)
                        }
                        None => match self.env.builtins.value(name) {
                            Some(entry) => {
                                let entry = entry.clone();
                                self.builtin_value(&entry, at)
                            }
                            // Base frames are also handlers; name the builtin the user wrote.
                            None if self.env.builtins.get(name).is_some() => {
                                self.ctx.diagnose(TypeErrorKind::BuiltinNotFirstClass {
                                    name: name.to_string(),
                                });
                                (self.ctx.unifier.fresh_ty(), Val::Unit)
                            }
                            None if self.env.lookup_handler(name).is_some() => {
                                self.ctx.diagnose(TypeErrorKind::HandlerNotFirstClass {
                                    name: name.to_string(),
                                });
                                (self.ctx.unifier.fresh_ty(), Val::Unit)
                            }
                            None if LANGUAGE_CONSTANTS.iter().any(|&(c, _)| c == &**name) => {
                                (Ty::Bool, Val::Variable(name.clone()))
                            }
                            None => {
                                self.diagnose_unbound(name);
                                (self.ctx.unifier.fresh_ty(), Val::Unit)
                            }
                        },
                    }
                }
            }
            Val::Thunk(comp) => {
                let (cty, comp) = self.with_scope(|this| this.infer_comp(comp.shape()));
                (Ty::Thunk(Box::new(cty)), Val::thunk(Arc::new(comp)))
            }
            Val::List(elems) => {
                let elem = self.ctx.unifier.fresh_ty();
                let nodes: Vec<_> = elems
                    .shape()
                    .iter()
                    .map(|entry| self.infer_list_entry(entry, false, &elem))
                    .collect();
                (Ty::list(elem), Val::list(nodes))
            }
            Val::Record(entries) => {
                let (ty, entries) = self.infer_record_val(entries.shape());
                (ty, Val::Record(entries))
            }
            Val::Map(entries) => {
                let (ty, entries) = self.infer_map_val(entries.shape());
                (ty, Val::Map(entries))
            }
            Val::Variant { label, payload } => {
                // Construction leaves the row open for other alternatives.
                let (payload_ty, payload) = self.infer_val(payload, None);
                let rest = self.ctx.unifier.fresh_row();
                (
                    Ty::Variant(Row::Extend(
                        Label::Case(label.to_string()),
                        Box::new(payload_ty),
                        Box::new(rest),
                    )),
                    Val::Variant {
                        label: label.clone(),
                        payload: Box::new(payload),
                    },
                )
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
    fn infer_pipeline(&mut self, stages: &[Arc<Comp>]) -> (CompTy, CompKind<Pending>) {
        // The parser unwraps a single-stage pipeline to the bare stage and the
        // elaborator preserves that shape, so a `Pipeline` node always has two.
        debug_assert!(stages.len() >= 2, "Pipeline carries ≥2 stages");

        let mut last = None;
        let mut nodes = Vec::new();
        for (position, stage) in stages.iter().enumerate() {
            if position > 0
                && let Some(feed) = stage_root_stdin_feed(stage)
            {
                self.with_span(stage.span, |this| {
                    this.ctx.diagnose(TypeErrorKind::DeadPipeEdge { feed });
                });
            }
            let (cty, node) = self.infer_comp(stage);
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
            last = Some(producer.into());
            nodes.push(Arc::new(node));
        }
        // The pipeline produces what its last stage does, at its own grade.
        (
            last.expect("≥2 stages by invariant above"),
            CompKind::Pipeline { stages: nodes },
        )
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
            CompKind::Force(v) => written(v).map(ToString::to_string),
            CompKind::App { head, .. } => match &head.item {
                CompKind::Force(v) => written(v).map(ToString::to_string),
                _ => None,
            },
            _ => None,
        }
    }

    fn infer_index(&mut self, target: &Val, key: &Spanned<Key>) -> (Ty, Prim<Pending>) {
        if let Some(literal) = spell_literal(target) {
            if let Key::Data(value) = &key.item {
                self.infer_val(value, None);
            }
            self.ctx.diagnose(TypeErrorKind::IndexOnLiteral {
                literal,
                key: spell_key(&key.item),
            });
            return (
                self.ctx.unifier.fresh_ty(),
                Prim::Index {
                    target: Val::Unit,
                    key: Spanned::with_span(key.span, Key::Position(0)),
                },
            );
        }
        let (target_ty, target) = self.infer_val(target, None);
        let (ty, key_node) = self.with_span(key.span, |this| {
            this.infer_index_step(&target_ty, &key.item)
        });
        (
            ty,
            Prim::Index {
                target,
                key: Spanned::with_span(key.span, key_node),
            },
        )
    }

    fn infer_index_step(&mut self, current_ty: &Ty, key: &Key) -> (Ty, Key<Pending>) {
        let elem = self.ctx.unifier.fresh_ty();
        let key = match key {
            Key::Label(label) => {
                let tail = self.ctx.unifier.fresh_row();
                let record = Ty::Record(Row::Extend(
                    Label::Field(label.to_string()),
                    Box::new(elem.clone()),
                    Box::new(tail),
                ));
                self.ctx
                    .unify_ty(current_ty, &record, Reason::RecordFieldRead);
                Key::Label(label.clone())
            }
            Key::Position(n) => {
                self.ctx.unify_ty(
                    current_ty,
                    &Ty::list(elem.clone()),
                    Reason::ListPositionRead,
                );
                Key::Position(*n)
            }
            Key::Data(value) => {
                let (key_ty, value) = self.infer_val(value, None);
                self.ctx
                    .unify_ty(current_ty, &Ty::map(elem.clone()), Reason::MapKeyRead);
                self.ctx.unify_ty(&key_ty, &Ty::String, Reason::MapIndexKey);
                Key::Data(value)
            }
        };
        (elem, key)
    }

    fn infer_case(
        &mut self,
        scrutinee: &crate::source::Spanned<Val>,
        arms: &[CaseArm],
    ) -> (CompTy, CompKind<Pending>) {
        let scrutinee_span = scrutinee.span;
        let (scrut_ty, scrutinee) = self.with_span(scrutinee_span, |this| {
            this.infer_val(&scrutinee.item, scrutinee_span)
        });

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

        // Arm payloads come from the scrutinee's row where known, so a
        // mismatch surfaces at the arm body's use; a residual `Var` here waits
        // for the final row-unify.
        let scrut_resolved_row = self.ctx.unifier.apply_row(&Row::Var(scrut_row_var));
        let scrut_payloads: std::collections::HashMap<Label, Ty> = scrut_resolved_row
            .fields()
            .map(|(l, t)| (l.clone(), t.clone()))
            .collect();

        // Exactly one arm runs, so they share one type: a literal `{ |p| … }`
        // binds its pattern to the known payload or a fresh type, and a thunk
        // in hand is unified. Payloads must be known before grades join.
        // Source order throughout, so a program's complaints arrive
        // in the order it was written.
        let payloads: Vec<Ty> = arms
            .iter()
            .map(|arm| {
                scrut_payloads
                    .get(&Label::Case(arm.tag.item.clone()))
                    .cloned()
                    .unwrap_or_else(|| self.ctx.unifier.fresh_ty())
            })
            .collect();
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
        let (joined, nodes) = self.join_arms(&join_arms, &Reason::CaseArms);
        let closed: Vec<(Label, Ty)> = arms
            .iter()
            .zip(&payloads)
            .map(|(arm, payload)| (Label::Case(arm.tag.item.clone()), payload.clone()))
            .collect();
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

        (
            joined,
            CompKind::Case {
                scrutinee: Spanned::with_span(scrutinee_span, scrutinee),
                arms: arms
                    .iter()
                    .zip(nodes)
                    .map(|(arm, body)| CaseArm {
                        tag: arm.tag.clone(),
                        body: Spanned::with_span(arm.body.span, body),
                    })
                    .collect(),
            },
        )
    }

    /// The `Rec` rule: bind each name to a self-referential mono
    /// computation type, infer every member in that recursive environment,
    /// unify each against its own type, unbind the self-bindings, and answer
    /// the `index`-th member's type beside its projection. Member types and
    /// the pending group are memoized in `ctx.rec_groups` per source `Arc`
    /// identity, so all projections share one inferred group.
    fn infer_rec(&mut self, group: &Arc<GroupNode>, index: usize) -> (CompTy, CompKind<Pending>) {
        let key = Arc::as_ptr(group);
        if let Some((betas, group)) = self.ctx.rec_groups.get(&key) {
            return (
                betas[index].clone(),
                CompKind::Rec {
                    group: Arc::clone(group),
                    index,
                },
            );
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
        let mut nodes = Vec::new();
        for ((name, member), beta) in members.iter().zip(betas.iter()) {
            let (member_ty, member) = self.infer_comp(member);
            self.ctx.unify_comp_ty(&member_ty, beta, Reason::LetRecSelf);
            nodes.push((name.clone(), Arc::new(member)));
        }
        for (name, _) in members {
            self.env.unbind(name);
        }
        let group = GroupNode::new(nodes.into());
        let cty = betas[index].clone();
        self.ctx.rec_groups.insert(key, (betas, Arc::clone(&group)));
        (cty, CompKind::Rec { group, index })
    }

    /// Note, for the `()` note, that `pattern` binds what a call returned.
    fn note_called(&mut self, pattern: &Pattern, rhs: &Comp) {
        if let Pattern::Name(name) = pattern
            && !is_synthetic(name)
            && let Some(callee) = Self::called_head(rhs)
        {
            self.env.note_called(name.to_string(), callee);
        }
    }

    /// The head of the call whose value a right-hand side ends in: an `App`, a
    /// `Force`, or a pipeline ending in one.
    fn called_head(rhs: &Comp) -> Option<String> {
        let mut rhs = rhs;
        while let CompKind::Bind { rest, .. } | CompKind::Seq { rest, .. } = &rhs.item {
            rhs = rest;
        }
        match &rhs.item {
            CompKind::Pipeline { stages } => stages.last().and_then(|s| Self::called_head(s)),
            CompKind::Force(v) => written(v).map(ToString::to_string),
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

    /// Hold the result, not its producer grade, to the retained contract.
    fn infer_protected_comp(
        &mut self,
        body: &Comp,
        site: &Arc<Site>,
    ) -> (CompTy, CompKind<Pending>) {
        let (have, body) = self.infer_comp(body);
        let result = self.force_return_shape(&have, Reason::ProtectContract).ty;
        let want = site.instantiate(&mut self.ctx.unifier);
        self.ctx.unify_ty(&result, &want, Reason::ProtectContract);
        (
            have,
            CompKind::Protect {
                body: Arc::new(body),
                site: Pending::Kept(Arc::clone(site)),
            },
        )
    }

    fn infer_prim(&mut self, prim: &Prim) -> (Ty, Prim<Pending>) {
        match prim {
            Prim::Binary(op, lhs, rhs) => self.infer_binary(*op, lhs, rhs),
            Prim::Negate(value) => {
                let (ty, value) = self.infer_val(value, None);
                let number = self.ctx.fresh_kinded(Kind::NUMBER);
                self.ctx.unify_ty(&ty, &number, Reason::Negation);
                (number, Prim::Negate(value))
            }
            Prim::Not(value) => self.infer_not(value),
            Prim::Index { target, key } => self.infer_index(target, key),
            Prim::Update { base, fields } => self.infer_update(base, fields),
            Prim::List(elems) => self.infer_list(elems),
            Prim::Map(entries) => self.infer_map(entries),
            Prim::Render(value) => {
                let (ty, value) = self.infer_val(value, None);
                let scalar = self.ctx.fresh_kinded(Kind::SCALAR);
                self.ctx.unify_ty(&ty, &scalar, Reason::Interpolation);
                (Ty::String, Prim::Render(value))
            }
            Prim::Concat(parts) => {
                let mut nodes = Vec::new();
                for value in parts {
                    let (ty, value) = self.infer_val(value, None);
                    self.ctx.unify_ty(&ty, &Ty::String, Reason::Interpolation);
                    nodes.push(value);
                }
                (Ty::String, Prim::Concat(nodes.into()))
            }
            Prim::Decode(value) => {
                let (ty, value) = self.infer_val(value, None);
                self.ctx.unify_ty(&ty, &Ty::Bytes, Reason::Capture);
                (Ty::String, Prim::Decode(value))
            }
        }
    }

    // Keep rule-local node temporaries off the recursive dispatch frame: in
    // debug builds every nested rule otherwise reserves their combined space.
    fn infer_app(
        &mut self,
        span: Option<Span>,
        head: &Comp,
        args: &[Spanned<Val>],
    ) -> (CompTy, CompKind<Pending>) {
        let (head_ty, head_node) = match &head.item {
            // The forced value's type is read here, not through the
            // `Force` rule, so a value used as a command head is named
            // as such before the general mismatch says it in jargon.
            CompKind::Force(value) => {
                let (ty, node) = self.with_span(head.span, |this| this.infer_val(value, None));
                if let Some(ty) = self.non_thunk_ty(&ty) {
                    match written(value) {
                        Some(name) => {
                            let kind = TypeErrorKind::HeadBoundToValue {
                                name: name.to_string(),
                                ty,
                            };
                            self.with_span(head.span, |this| this.ctx.diagnose(kind));
                        }
                        None => self.ctx.diagnose(TypeErrorKind::CommandNotFunction { ty }),
                    }
                    // Check the args anyway, then hand the enclosing
                    // pipeline or chain a coherent fresh result.
                    self.infer_refused_args(args);
                    return (
                        self.ctx.unifier.fresh_comp_ty(),
                        CompKind::Return(Val::Unit),
                    );
                }
                let cty = self.with_span(head.span, |this| {
                    this.forced(value, &ty, Reason::AutoderefHead)
                });
                (cty, Spanned::with_span(head.span, CompKind::Force(node)))
            }
            _ => self.with_span(head.span, |this| this.infer_comp(head)),
        };
        let (cty, args, splits) = self.apply_args(head_ty, args);
        let node = self.split(span, &splits, args, |args| applied(span, head_node, args));
        (cty, node)
    }

    fn infer_exec(&mut self, comp: &Comp, e: &Exec) -> (CompTy, CompKind<Pending>) {
        let redirects = e.redirects.map(|operand| self.infer_val(operand, None).1);
        let (cty, node) = match &e.head {
            CommandWord::Name(CommandName::Bare(name)) => {
                self.exec_comp_ty(comp, name, e, redirects)
            }
            CommandWord::External(name)
            | CommandWord::Name(name @ (CommandName::Path(_) | CommandName::TildePath(_))) => {
                let (cty, argv) = self.external_exec_comp_ty(&name.written(), &e.argv);
                (cty, exec_node(&e.head, argv, redirects, None))
            }
        };
        let cty = if e.redirects.is_empty() {
            cty
        } else {
            self.redirected(&cty, e.redirects.stdout.is_some())
        };
        (cty, node)
    }

    fn infer_if(
        &mut self,
        cond: &Spanned<Val>,
        then: &Spanned<Val>,
        else_: &Spanned<Val>,
    ) -> (CompTy, CompKind<Pending>) {
        let (cond_ty, cond_node) = self.infer_val(&cond.item, cond.span);
        // Underline just the cond, not the whole `if … else …` form.
        self.with_span(cond.span, |this| {
            this.ctx.unify_ty(&cond_ty, &Ty::Bool, Reason::IfCond);
        });
        let arms = [then, else_].map(|arm| JoinArm::new(arm, vec![], Reason::IfBranches));
        let (cty, nodes) = self.join_arms(&arms, &Reason::IfBranches);
        let mut nodes = nodes.into_iter();
        (
            cty,
            CompKind::If {
                cond: Spanned::with_span(cond.span, cond_node),
                then: Spanned::with_span(then.span, nodes.next().expect("then arm")),
                else_: Spanned::with_span(else_.span, nodes.next().expect("else arm")),
            },
        )
    }

    pub(super) fn infer_comp(&mut self, comp: &Comp) -> (CompTy, Comp<Pending>) {
        if let Some(span) = comp.span {
            self.ctx.pos = Some(span);
        }

        let (cty, item) = match &comp.item {
            CompKind::Protect { body, site } => self.infer_protected_comp(body, site),
            CompKind::Return(value) => {
                let (ty, value) = self.infer_val(value, None);
                (self.produces(ty), CompKind::Return(value))
            }
            CompKind::Lam { param, body } => {
                let param_ty = self.ctx.unifier.fresh_ty();
                let (body_ty, body) = self.with_scope(|this| {
                    this.env
                        .bind(param.to_string(), Scheme::mono(param_ty.clone()));
                    this.infer_comp(body)
                });
                (
                    CompTy::Fun(Box::new(param_ty), Box::new(body_ty)),
                    CompKind::Lam {
                        param: param.clone(),
                        body: Arc::new(body),
                    },
                )
            }
            CompKind::Force(value) => {
                let (val_ty, node) = self.infer_val(value, None);
                (
                    self.forced(value, &val_ty, Reason::ForceOperand),
                    CompKind::Force(node),
                )
            }
            CompKind::Bind {
                comp: inner,
                pattern,
                rest,
            } => {
                let inner = self.infer_let(pattern, inner);
                let (cty, rest) = self.infer_comp(rest);
                (
                    cty,
                    CompKind::Bind {
                        comp: inner,
                        pattern: Arc::clone(pattern),
                        rest: Arc::new(rest),
                    },
                )
            }
            CompKind::Seq { first, rest } => {
                let (_, first) = self.infer_statement(first);
                let (cty, rest) = self.infer_comp(rest);
                (
                    cty,
                    CompKind::Seq {
                        first: Arc::new(first),
                        rest: Arc::new(rest),
                    },
                )
            }
            CompKind::App { head, args } => self.infer_app(comp.span, head, args),
            CompKind::Exec(exec) => self.infer_exec(comp, exec),
            CompKind::Native { .. } => {
                unreachable!("a native's code is built by forcing it, never checked")
            }
            CompKind::Pipeline { stages } => self.infer_pipeline(stages),
            CompKind::Prim(prim) => {
                let (ty, prim) = self.infer_prim(prim);
                (self.produces(ty), CompKind::Prim(prim))
            }
            CompKind::Rec { group, index } => self.infer_rec(group, *index),
            CompKind::Tilde(path) => (self.produces(Ty::String), CompKind::Tilde(path.clone())),
            CompKind::If { cond, then, else_ } => self.infer_if(cond, then, else_),
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
            CompKind::Spawn { title, body } => self.infer_spawn(title.as_ref(), body),
            CompKind::Gather { body } => self.infer_gather(body),
            // Installs fds, not its own type; carries the body as an
            // `Arc<Comp>` rather than a thunk-shaped `Val` like the scope
            // forms above, so infer it directly.
            CompKind::Redirect { body, redirects } => {
                let redirects = redirects.map(|operand| self.infer_val(operand, None).1);
                let (cty, body) = self.infer_comp(body);
                (
                    self.redirected(&cty, redirects.stdout.is_some()),
                    CompKind::Redirect {
                        body: Arc::new(body),
                        redirects,
                    },
                )
            }
            // Rechecked live closures can already contain captures.
            CompKind::Capture(body) => {
                let (body_ty, body) = self.infer_comp(body);
                self.ctx
                    .unify_comp_ty(&body_ty, &CompTy::command(), Reason::Capture);
                (self.produces(Ty::Bytes), CompKind::Capture(Arc::new(body)))
            }
        };
        (cty, Spanned::with_span(comp.span, item))
    }
}

#[cfg(test)]
mod tests {
    use super::InferCtx;
    use super::{TypeErrorKind, captured_string, coverage_verdict};
    use crate::ir::{Comp, CompKind, Prim, Val};
    use crate::source::{FileId, Span, Spanned};
    use crate::syntax::parser::parse;
    use crate::ty::{Label, Row, RowVar, Scheme, Ty};
    use crate::{SessionSchemes, elaborator::elaborate, typecheck};
    use std::sync::Arc;

    #[test]
    fn capture_nodes_take_the_bodys_span() {
        let span = Span::new(FileId::DUMMY, 3, 9);
        let mut ctx = InferCtx::new();

        let body: Comp = Spanned::new(span, CompKind::Return(Val::Unit));
        let CompKind::Bind { comp, rest, .. } = captured_string(body, &mut ctx) else {
            panic!("a capture is bound to its decode");
        };
        assert!(matches!(comp.item, CompKind::Capture(_)));
        assert!(matches!(rest.item, CompKind::Prim(Prim::Decode(_))));
        assert_eq!([comp.span, rest.span], [Some(span); 2]);
    }

    /// The missing and the extra tags a case is judged to have.
    type Verdict<'a> = Option<(&'a [&'a str], &'a [&'a str])>;

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
        let comp = elaborate(&ast, [], None)
            .unwrap_or_else(|e| panic!("elaborate error in {src:?}: {e:?}"));
        let schemes = SessionSchemes {
            bindings,
            ..crate::test_helper::core_schemes()
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
    /// with a spread (`Prim::Map`) alike.
    #[test]
    fn a_map_literal_refuses_a_static_duplicate_key() {
        assert_eq!(error_codes("return [a: 1, a: 2]"), ["T0022"]);
        assert_eq!(error_codes("return [:, a: 1, a: 2]"), ["T0022"]);
        assert_eq!(
            error_codes("let k = 'x'\nreturn [$k: 1, a: 1, a: 2]"),
            ["T0022"]
        );
    }

    /// Missing arms are named always; extra arms only where the value's row is
    /// closed, an open tail being free to absorb one.
    #[test]
    fn coverage_names_missing_arms_and_extra_arms_of_a_closed_row_only() {
        let row = |tags: &[&str], tail: Row| {
            tags.iter().rev().fold(tail, |rest, tag| {
                Row::Extend(
                    Label::Case((*tag).into()),
                    Box::new(Ty::Unit),
                    Box::new(rest),
                )
            })
        };
        let arms = |tags: &[&str]| -> Vec<Label> {
            tags.iter().map(|t| Label::Case((*t).into())).collect()
        };
        let strings =
            |tags: &[&str]| -> Vec<String> { arms(tags).iter().map(Label::to_string).collect() };
        let open = || Row::Var(RowVar(0));
        let cases: [(Row, &[&str], Verdict); 6] = [
            (row(&["a", "b"], Row::Empty), &["a", "b"], None),
            (row(&["a", "b"], Row::Empty), &["a"], Some((&["b"], &[]))),
            (row(&["a"], Row::Empty), &["a", "b"], Some((&[], &["b"]))),
            (
                row(&["a"], Row::Empty),
                &["b", "c"],
                Some((&["a"], &["b", "c"])),
            ),
            (row(&["a"], open()), &["a", "b"], None),
            (row(&["a", "b"], open()), &["a", "c"], Some((&["b"], &[]))),
        ];
        for (scrutinee, arm_tags, expected) in cases {
            let got = coverage_verdict(&scrutinee, &arms(arm_tags)).map(|k| match k {
                TypeErrorKind::CaseNotExhaustive { missing, extra } => (missing, extra),
                other => panic!("not a coverage verdict: {other:?}"),
            });
            let want = expected.map(|(missing, extra)| (strings(missing), strings(extra)));
            assert_eq!(got, want, "{scrutinee:?} against {arm_tags:?}");
        }
    }
}
