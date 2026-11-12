use super::{
    Arc, Argv, Ast, CommandName, CommandWord, Comp, CompKind, Elaborator, Exec, Head, ParseError,
    Pattern, Redirects, Spanned, Val, ValListElem, WithSpan,
};

impl Elaborator<'_> {
    /// Every name-dispatched head (`bare`, `^name`, `./path`, `~/path`) funnels
    /// through here.
    fn exec(&self, head: CommandWord, argv: Argv, redirects: Redirects<Val>) -> Comp {
        comp!(
            self,
            CompKind::Exec(Exec {
                head,
                argv,
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
        // A value head runs before the arguments' hoists, whatever its type.
        let value_head = if let Head::Value(value) = &head.item {
            Some(self.with_span(head.span, |this| {
                let value = this.to_val(value, binds);
                comp!(this, CompKind::Force(value))
            }))
        } else {
            None
        };

        // Only an argument-position spread splices here: a list-literal
        // argument `f [...$xs]` stays one arg carrying its own spread.
        let argv: Argv = args
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
                argv,
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
                self.apply_head(head_comp, argv, redirect_vals)
            }
            Head::Bare(s) => {
                self.with_span(head.span, |this| this.refer(s));
                self.exec(
                    CommandWord::Name(CommandName::Bare(s.clone().into())),
                    desugar_zero_arg_exit(s, argv),
                    redirect_vals,
                )
            }
            Head::Path(path) => self.exec(
                CommandWord::Name(CommandName::Path(path.clone())),
                argv,
                redirect_vals,
            ),
            Head::TildePath(path) => self.exec(
                CommandWord::Name(CommandName::TildePath(path.clone())),
                argv,
                redirect_vals,
            ),
            Head::Value(_) => {
                let head_comp = value_head.expect("lowered above for Head::Value");
                self.apply_head(head_comp, argv, redirect_vals)
            }
        }
    }

    /// Shared by the two value-application heads, a bound bare name (`f x`) and
    /// an explicit value head (`$f x`, `{…} x`), each forced.  A zero-arg call
    /// is the head computation alone: `App` with an empty argument list is not
    /// a CBPV form.  A value takes its arguments by application, so it has no
    /// argv for `...` to spread into.
    fn apply_head(&mut self, head_comp: Comp, argv: Argv, redirects: Redirects<Val>) -> Comp {
        let args: Box<[Spanned<Val>]> = argv
            .into_iter()
            .map(|elem| match elem {
                ValListElem::Single(arg) => arg,
                ValListElem::Spread(arg) => {
                    let (message, hint) = crate::typecheck::spread_into_application();
                    self.refuse(ParseError::new(arg.span, message).with_hint(hint));
                    arg
                }
            })
            .collect();
        let app = if args.is_empty() {
            head_comp
        } else {
            comp!(
                self,
                CompKind::App {
                    head: Arc::new(head_comp),
                    args,
                    hole: (),
                }
            )
        };
        self.wrap_redirect(app, redirects)
    }

    /// Attach trailing `redirects` to `body` as a [`CompKind::Redirect`] frame.
    /// `Exec` carries its own redirects instead.
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
fn desugar_zero_arg_exit(name: &str, argv: Argv) -> Argv {
    if argv.is_empty() && (name == "exit" || name == "quit") {
        Argv::from(vec![ValListElem::Single(Spanned::synthetic(Val::Int(0)))])
    } else {
        argv
    }
}
