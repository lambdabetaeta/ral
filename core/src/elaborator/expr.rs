use super::branches::Junction;
use super::{
    Arc, Assembly, Ast, Comp, CompKind, Elaborator, HandlerArmV, ListElem, MapEntry, MapEntryV,
    MapOperand, Name, Options, OptionsV, Pattern, RecordEntry, Redirects, ScopeAst, Span, Spanned,
    Val, ValListElem, ValRecordEntry, WithSpan, Word, word_val,
};

impl Elaborator {
    /// Elaborate `ast` as a computation, pushing every sub-expression that must
    /// run before its parent into `binds` for the caller to `wrap_binds`.
    pub(super) fn elab_expr(&mut self, ast: &Ast, binds: &mut Vec<(Pattern, Comp)>) -> Comp {
        match ast {
            Ast::Word(Word::Plain(s) | Word::Path(s)) => {
                comp!(self, CompKind::Return(word_val(s)))
            }
            Ast::Literal(s) => comp!(self, CompKind::Return(Val::String(s.clone().into()))),
            Ast::Variable(s) => {
                comp!(self, CompKind::Return(self.variable_val(s)))
            }
            Ast::Word(Word::Tilde(path)) => {
                let tilde = comp!(self, CompKind::Tilde(path.clone()));
                let v = self.hoist(tilde, binds);
                comp!(self, CompKind::Return(v))
            }

            Ast::Block(body) => {
                let body_comp = self.with_new_scope(|this| this.stmts_nested(body));
                comp!(self, CompKind::Return(Val::thunk(Arc::new(body_comp))))
            }

            Ast::Lambda { param, body } => {
                let (param_ir, body_comp) = self.elab_binder_scope(param, body);
                // A body that is itself a single lambda already carries its own
                // thunk; reuse it rather than wrapping a thunk in a thunk.
                let body_arc: Arc<Comp> = match &body_comp.item {
                    CompKind::Return(Val::Thunk(inner))
                        if matches!(inner.shape().item, CompKind::Lam { .. }) =>
                    {
                        Arc::clone(inner.shape())
                    }
                    _ => Arc::new(body_comp),
                };
                comp!(
                    self,
                    CompKind::Return(Val::thunk(Arc::new(comp!(
                        self,
                        CompKind::Lam {
                            param: param_ir,
                            body: body_arc,
                        }
                    ))))
                )
            }

            Ast::Force(value) => self.with_span(value.span, |this| {
                comp!(this, CompKind::Force(this.to_val(&value.item, binds)))
            }),

            Ast::Call { head, args } => self.elab_call(head, args, &Redirects::default(), binds),

            // The stage's own operands lower first, then its redirects.
            Ast::Redirected { stage, redirects } => match &*stage.item {
                Ast::Call { head, args } => self.with_span(stage.span, |this| {
                    this.elab_call(head, args, redirects, binds)
                }),
                other => {
                    let body = self.with_span(stage.span, |this| this.elab_expr(other, binds));
                    let redirects = self.lower_redirects(redirects, binds);
                    self.wrap_redirect(body, redirects)
                }
            },

            Ast::Scope(op) => match op {
                ScopeAst::Try { body, handler } => comp!(
                    self,
                    CompKind::Try {
                        body: self.to_val(body, binds),
                        handler: self.to_val(handler, binds),
                    }
                ),
                ScopeAst::Guard { body, cleanup } => comp!(
                    self,
                    CompKind::Guard {
                        body: self.to_val(body, binds),
                        cleanup: self.to_val(cleanup, binds),
                    }
                ),
                ScopeAst::Within {
                    opts,
                    handlers,
                    body,
                } => {
                    let opts = self.lower_options(opts, binds);
                    let handlers = handlers.as_ref().map(|arms| {
                        arms.iter()
                            .map(|arm| HandlerArmV {
                                name: arm.name.clone(),
                                value: Spanned::with_span(
                                    arm.value.span,
                                    self.to_val(&arm.value.item, binds),
                                ),
                            })
                            .collect()
                    });
                    comp!(
                        self,
                        CompKind::Within {
                            opts,
                            handlers,
                            body: self.to_val(body, binds),
                        }
                    )
                }
                ScopeAst::Grant { caps, body } => comp!(
                    self,
                    CompKind::Grant {
                        caps: self.lower_options(caps, binds),
                        body: self.to_val(body, binds),
                    }
                ),
                ScopeAst::Audit { body } => comp!(
                    self,
                    CompKind::Audit {
                        body: self.to_val(body, binds),
                    }
                ),
                ScopeAst::Spawn { title, body } => comp!(
                    self,
                    CompKind::Spawn {
                        title: title.as_deref().map(|t| self.spanned_val(t, binds)),
                        body: self.to_val(body, binds),
                    }
                ),
                ScopeAst::Gather { body } => comp!(
                    self,
                    CompKind::Gather {
                        body: self.to_val(body, binds),
                    }
                ),
            },

            // `return` with no value and `()` name the same value.
            Ast::Unit | Ast::Return(None) => comp!(self, CompKind::Return(Val::Unit)),

            Ast::Return(Some(value)) => self.with_span(value.span, |this| {
                comp!(this, CompKind::Return(this.to_val(&value.item, binds)))
            }),

            Ast::Pipeline(stages) => {
                let mut comps = Vec::new();
                for stage in stages {
                    // A `{ … }` stage is the thunk the pipeline drives, not
                    // inline statements — hence `elab_isolated`, which isolates
                    // the stage's hoists without `elab_guarded`'s inline-block
                    // reading.
                    let stage_comp =
                        self.with_span(stage.span, |this| this.elab_isolated(&stage.item));
                    comps.push(Arc::new(stage_comp));
                }
                comp!(self, CompKind::Pipeline { stages: comps })
            }

            Ast::Chain(parts) => {
                // `a ? b ? c` is `try a { |_| try b { |_| c } }`, right-nested
                // so the last arm stays in tail position.  Every arm but the
                // first runs only when its predecessors failed, so each needs
                // the fresh binds vector `elab_guarded` gives it.
                let mut arms: Vec<(Option<Span>, Comp)> = parts
                    .iter()
                    .map(|a| {
                        (
                            a.span,
                            self.with_span(a.span, |this| this.elab_guarded(&a.item)),
                        )
                    })
                    .collect();
                match arms.pop() {
                    None => comp!(self, CompKind::Return(Val::Unit)),
                    Some((_, last)) => {
                        arms.into_iter()
                            .rev()
                            .fold(last, |handler_body, (span, arm)| {
                                self.with_span(span, |this| {
                                    let handler = Val::thunk(Arc::new(comp!(
                                        this,
                                        CompKind::Lam {
                                            param: Pattern::Wildcard,
                                            body: Arc::new(handler_body),
                                        }
                                    )));
                                    comp!(
                                        this,
                                        CompKind::Try {
                                            body: Val::thunk(Arc::new(arm)),
                                            handler,
                                        }
                                    )
                                })
                            })
                    }
                }
            }

            Ast::List(elems) => {
                let plain: Option<Vec<&Spanned<Ast>>> = elems
                    .iter()
                    .map(|e| match e {
                        ListElem::Single(a) => Some(a),
                        ListElem::Spread(_) => None,
                    })
                    .collect();
                match plain {
                    Some(items) => comp!(
                        self,
                        CompKind::Return(Val::list(
                            items
                                .into_iter()
                                .map(|a| self.spanned_val(a, binds))
                                .collect::<Vec<_>>(),
                        ))
                    ),
                    None => comp!(
                        self,
                        CompKind::Assemble(Assembly::List(
                            elems
                                .iter()
                                .map(|e| match e {
                                    ListElem::Single(a) => {
                                        ValListElem::Single(self.spanned_val(a, binds))
                                    }
                                    ListElem::Spread(a) => {
                                        ValListElem::Spread(self.spanned_val(a, binds))
                                    }
                                })
                                .collect(),
                        ))
                    ),
                }
            }

            Ast::Record(entries) => {
                let plain: Option<Vec<(&String, &Spanned<Ast>)>> = entries
                    .iter()
                    .map(|e| match e {
                        RecordEntry::Field { key, value } => Some((&key.item, value)),
                        RecordEntry::Spread(_) => None,
                    })
                    .collect();
                match plain {
                    Some(items) => {
                        let fields = self.sorted_fields(items, binds);
                        comp!(self, CompKind::Return(Val::record(fields)))
                    }
                    None => comp!(
                        self,
                        CompKind::Assemble(Assembly::Record(
                            entries
                                .iter()
                                .map(|e| match e {
                                    RecordEntry::Field { key, value } => ValRecordEntry::Field(
                                        key.item.clone(),
                                        self.spanned_val(value, binds),
                                    ),
                                    RecordEntry::Spread(a) => {
                                        ValRecordEntry::Spread(self.spanned_val(a, binds))
                                    }
                                })
                                .collect(),
                        ))
                    ),
                }
            }

            Ast::Map(entries) => {
                let plain: Option<Vec<(&String, &Spanned<Ast>)>> = entries
                    .iter()
                    .map(|e| match e {
                        MapEntry::Entry { key, value } => match &key.item {
                            Ast::Literal(key) => Some((key, value)),
                            _ => None,
                        },
                        MapEntry::Spread(_) => None,
                    })
                    .collect();
                match plain {
                    Some(items) => {
                        let fields = self.sorted_fields(items, binds);
                        comp!(self, CompKind::Return(Val::map(fields)))
                    }
                    None => comp!(
                        self,
                        CompKind::Assemble(Assembly::Map(
                            entries
                                .iter()
                                .map(|e| match e {
                                    MapEntry::Entry { key, value } => MapEntryV::Entry(
                                        self.map_operand(key),
                                        self.map_operand(value),
                                    ),
                                    MapEntry::Spread(a) => {
                                        MapEntryV::Spread(self.map_operand(a))
                                    }
                                })
                                .collect(),
                        ))
                    ),
                }
            }

            Ast::Tag { label, payload } => {
                let payload_val = payload
                    .as_ref()
                    .map(|p| self.with_span(p.span, |this| Box::new(this.to_val(&p.item, binds))));
                comp!(
                    self,
                    CompKind::Return(Val::Variant {
                        label: label.as_str().into(),
                        payload: payload_val,
                    })
                )
            }

            Ast::Interpolation(parts) => {
                comp!(
                    self,
                    CompKind::Interpolation(
                        parts
                            .iter()
                            .map(|a| self.with_span(a.span, |this| this.to_val(&a.item, binds)))
                            .collect()
                    )
                )
            }

            Ast::Binary(l, op, r) => {
                let lv = self.with_span(l.span, |this| this.to_val(&l.item, binds));
                let rv = self.with_span(r.span, |this| this.to_val(&r.item, binds));
                comp!(self, CompKind::Binary(*op, lv, rv))
            }
            Ast::Negate(inner) => {
                let v = self.with_span(inner.span, |this| this.to_val(&inner.item, binds));
                comp!(self, CompKind::Negate(v))
            }
            Ast::Not(inner) => {
                let v = self.with_span(inner.span, |this| this.to_val(&inner.item, binds));
                comp!(self, CompKind::Not(v))
            }
            Ast::And(l, r) => self.lower_short_circuit(l, r, binds, Junction::And),
            Ast::Or(l, r) => self.lower_short_circuit(l, r, binds, Junction::Or),

            Ast::Index { target, keys } => comp!(
                self,
                CompKind::Index {
                    target: self
                        .with_span(target.span, |this| { this.to_val(&target.item, binds) }),
                    keys: keys
                        .iter()
                        .map(|k| Spanned::with_span(
                            k.span,
                            self.with_span(k.span, |this| this.to_val(&k.item, binds)),
                        ))
                        .collect(),
                }
            ),

            Ast::If { branches, else_ } => self.elab_if(branches, else_.as_ref(), binds),

            // The scrutinee always runs, so whatever it hoists joins the
            // caller's binds; an arm may not run, and its body is a closed
            // computation that hoists nothing outward.
            Ast::Case { scrutinee, arms } => comp!(
                self,
                CompKind::Case {
                    scrutinee: Spanned::with_span(
                        scrutinee.span,
                        self.with_span(scrutinee.span, |this| {
                            this.to_val(&scrutinee.item, binds)
                        }),
                    ),
                    arms: arms.iter().map(|arm| self.elab_case_arm(arm)).collect(),
                }
            ),

            Ast::Let { .. } => unreachable!("assignment in elab_expr"),

            Ast::Spread(_) => {
                unreachable!("Ast::Spread must be consumed by Ast::Call's arg lowering")
            }
        }
    }

    fn map_operand(&mut self, ast: &Spanned<Ast>) -> MapOperand {
        let comp = self.with_span(ast.span, |this| this.elab_isolated(&ast.item));
        match comp.item {
            CompKind::Return(value) => MapOperand::Value(Spanned::with_span(ast.span, value)),
            _ => MapOperand::Compute(Arc::new(comp)),
        }
    }

    /// [`Self::to_val`], spanned at `ast`'s own range — every literal
    /// element and entry value.
    fn spanned_val(
        &mut self,
        ast: &Spanned<Ast>,
        binds: &mut Vec<(Pattern, Comp)>,
    ) -> Spanned<Val> {
        Spanned::with_span(
            ast.span,
            self.with_span(ast.span, |this| this.to_val(&ast.item, binds)),
        )
    }

    /// A plain record or map literal's entries, elaborated in written order and
    /// then sorted by key, stably.
    fn sorted_fields(
        &mut self,
        items: Vec<(&String, &Spanned<Ast>)>,
        binds: &mut Vec<(Pattern, Comp)>,
    ) -> Vec<(Name, Spanned<Val>)> {
        let mut fields: Vec<(Name, _)> = items
            .into_iter()
            .map(|(key, value)| (key.as_str().into(), self.spanned_val(value, binds)))
            .collect();
        fields.sort_by(|(a, _), (b, _)| a.cmp(b));
        fields
    }

    /// A form's written options, each value hoisted like any other value.
    fn lower_options(&mut self, opts: &Options, binds: &mut Vec<(Pattern, Comp)>) -> OptionsV {
        opts.iter()
            .map(|(name, value)| (Name::from(name.as_str()), self.spanned_val(value, binds)))
            .collect()
    }
}
