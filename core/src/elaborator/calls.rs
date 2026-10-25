use super::{
    Arc, Args, Ast, CommandName, CommandWord, Comp, CompKind, Elaborator, Exec, Head, Pattern,
    Redirects, Spanned, Val, ValListElem, WithSpan,
};

impl Elaborator {
    /// Every name-dispatched head (`bare`, `^name`, `./path`, `~/path`) funnels
    /// through here.
    fn exec(&self, head: CommandWord, args: Args, redirects: Redirects<Val>) -> Comp {
        comp!(
            self,
            CompKind::Exec(Exec {
                head,
                args,
                redirects,
                site: None,
            })
        )
    }

    /// A head applied to `args`, with the stage's trailing `redirects`.
    pub(super) fn elab_call(
        &mut self,
        head: &Spanned<Head>,
        args: &[Spanned<Ast>],
        redirects: &Redirects<Ast>,
        binds: &mut Vec<(Pattern, Comp)>,
    ) -> Comp {
        // A value head can hoist binds of its own, and its effects
        // precede the arguments' in source order — so it is elaborated
        // before the args below, or its binds land after theirs.
        let value_head_comp = if let Head::Value(value) = &head.item {
            Some(self.with_span(head.span, |this| this.elab_expr(value, binds)))
        } else {
            None
        };

        // Only an argument-position spread splices here: a list-literal
        // argument `f [...$xs]` stays one arg carrying its own spread.
        let arg_vals: Args = args
            .iter()
            .map(|a| match &a.item {
                Ast::Spread(inner) => ValListElem::Spread(Spanned::with_span(
                    a.span,
                    self.with_span(a.span, |this| this.to_val(&inner.item, binds)),
                )),
                other => ValListElem::Single(Spanned::with_span(
                    a.span,
                    self.with_span(a.span, |this| this.to_val(other, binds)),
                )),
            })
            .collect();
        let redirect_vals = self.lower_redirects(redirects, binds);

        match &head.item {
            Head::ExternalName(s) => self.exec(
                CommandWord::External(CommandName::Bare(s.clone().into())),
                arg_vals,
                redirect_vals,
            ),
            Head::Bare(s) if self.is_bound(s) => {
                // The `Force` is what makes the `Force` rule run a
                // bound block inside the redirect frame wrapped
                // around it.
                let head_comp = self.with_span(head.span, |this| {
                    this.refer(s);
                    comp!(this, CompKind::Force(Val::Variable(s.clone().into())))
                });
                self.apply_head(head_comp, arg_vals, redirect_vals)
            }
            Head::Bare(s) => {
                self.with_span(head.span, |this| this.refer(s));
                self.exec(
                    CommandWord::Name(CommandName::Bare(s.clone().into())),
                    desugar_zero_arg_exit(s, arg_vals),
                    redirect_vals,
                )
            }
            Head::Path(path) => self.exec(
                CommandWord::Name(CommandName::Path(path.clone())),
                arg_vals,
                redirect_vals,
            ),
            Head::TildePath(path) => self.exec(
                CommandWord::Name(CommandName::TildePath(path.clone())),
                arg_vals,
                redirect_vals,
            ),
            Head::Value(_) => {
                let head_comp = value_head_comp.expect("computed above for Head::Value");
                self.apply_head(head_comp, arg_vals, redirect_vals)
            }
        }
    }

    /// Shared by the two value-application heads, a bound bare name (`f x`) and
    /// an explicit value head (`$f x`, `{…} x`).  A zero-arg call is the head
    /// computation alone: `App` with an empty argument list is not a CBPV form.
    fn apply_head(&self, head_comp: Comp, arg_vals: Args, redirects: Redirects<Val>) -> Comp {
        let app = if arg_vals.is_empty() {
            head_comp
        } else {
            comp!(
                self,
                CompKind::App {
                    head: Arc::new(head_comp),
                    args: arg_vals,
                }
            )
        };
        self.wrap_redirect(app, redirects)
    }

    /// Attach trailing `redirects` to `body` as a [`CompKind::Redirect`] frame.
    /// `Exec` carries its own redirects instead, and pipelines and chains take
    /// X
    pub(super) fn wrap_redirect(&self, body: Comp, redirects: Redirects<Val>) -> Comp {
        if redirects.is_empty() {
            return body;
        }
        comp!(
            self,
            CompKind::Redirect {
                body: Arc::new(body),
                redirects,
            }
        )
    }

    /// Lower each redirect's operand, hoisting an effectful one into `binds`
    /// like any other value.
    pub(super) fn lower_redirects(
        &mut self,
        redirects: &Redirects<Ast>,
        binds: &mut Vec<(Pattern, Comp)>,
    ) -> Redirects<Val> {
        redirects.map(|a| self.to_val(a, binds))
    }
}

/// Sugar bare `exit` / `quit` into `exit 0` / `quit 0`.  The builtin tolerates
/// zero args, but ral's fixed-arity rule gives it one `Int` slot; supplying the
/// status here spares the typechecker a zero-arg special case.
fn desugar_zero_arg_exit(name: &str, args: Args) -> Args {
    if args.is_empty() && (name == "exit" || name == "quit") {
        Args::from(vec![ValListElem::Single(Spanned::synthetic(Val::Int(0)))])
    } else {
        args
    }
}
