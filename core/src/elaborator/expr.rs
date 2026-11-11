use super::branches::Junction;
use super::{
    Arc, Ast, Comp, CompKind, Elaborator, ListElem, MapEntry, MapEntryV, Name, Pattern,
    RecordEntry, Redirects, ScopeAst, Span, Spanned, Val, ValListElem, WithSpan, Word, word_val,
};

use crate::ir::{FieldsNode, Key, Prim};
use crate::syntax::ast::{IndexKey, Options, Stmt};
use std::collections::HashSet;

impl Elaborator {
    fn lambda_param(
        &mut self,
        pattern: &Pattern,
        destructured: &mut HashSet<Name>,
    ) -> (Name, Option<Pattern>) {
        match pattern {
            Pattern::Name(name) if !destructured.contains(name) => (name.clone(), None),
            Pattern::Wildcard => (self.gensym(), None),
            _ => {
                destructured.extend(pattern.names().into_iter().cloned());
                (self.gensym(), Some(pattern.clone()))
            }
        }
    }

    fn elab_lambda(&mut self, param: &Spanned<Pattern>, body: &[Stmt]) -> Comp {
        let mut params = vec![(self.current_span, param)];
        let mut body = body;
        let mut body_span = self.current_span;
        while let [stmt] = body {
            let (span, ast) = match &stmt.item {
                Ast::Return(Some(value)) => (value.span, value.item.as_ref()),
                ast => (stmt.span, ast),
            };
            let Ast::Lambda { param, body: inner } = ast else {
                break;
            };
            params.push((span, param));
            body = inner;
            body_span = span;
        }
        let body = self.with_new_scope(|this| {
            for (_, param) in &params {
                this.bind_pattern(param);
            }
            this.with_span(body_span, |this| this.stmts_nested(body))
        });
        let mut destructured = HashSet::new();
        let params: Vec<_> = params
            .into_iter()
            .map(|(span, param)| {
                let (name, pattern) = self.lambda_param(&param.item, &mut destructured);
                (span, param.span, name, pattern)
            })
            .collect();
        let body = params
            .iter()
            .rev()
            .fold(body, |rest, (_, span, name, pattern)| match pattern {
                Some(pattern) => Spanned::with_span(
                    *span,
                    CompKind::bind(
                        pattern.clone(),
                        Spanned::with_span(*span, CompKind::Return(Val::Variable(name.clone()))),
                        rest,
                    ),
                ),
                None => rest,
            });
        let lambda = params
            .into_iter()
            .rev()
            .fold(body, |body, (span, _, param, _)| {
                Spanned::with_span(
                    span,
                    CompKind::Lam {
                        param,
                        body: Arc::new(body),
                    },
                )
            });
        comp!(self, CompKind::Return(Val::thunk(Arc::new(lambda))))
    }

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

            Ast::Lambda { param, body } => self.elab_lambda(param, body),

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
                ScopeAst::Guard {
                    acquire,
                    body,
                    release,
                } => comp!(
                    self,
                    CompKind::Guard {
                        acquire: self.to_val(acquire, binds),
                        body: self.to_val(body, binds),
                        release: self.to_val(release, binds),
                    }
                ),
                ScopeAst::Within { opts, body } => comp!(
                    self,
                    CompKind::Within {
                        opts: self.options(opts, binds),
                        body: self.to_val(body, binds),
                    }
                ),
                ScopeAst::Grant { caps, body } => comp!(
                    self,
                    CompKind::Grant {
                        caps: self.options(caps, binds),
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
                                            param: this.gensym(),
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
                        CompKind::Prim(Prim::List(
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
                if let Some(items) = plain {
                    let fields = self.sorted_fields(items, binds);
                    comp!(self, CompKind::Return(Val::record(fields)))
                } else {
                    let Some((RecordEntry::Spread(base), fields)) = entries.split_first() else {
                        unreachable!("a record update starts with its base")
                    };
                    let base = self.spanned_val(base, binds);
                    let fields = fields
                        .iter()
                        .map(|entry| {
                            let RecordEntry::Field { key, value } = entry else {
                                unreachable!("a record update has one base")
                            };
                            (key.item.clone().into(), self.spanned_val(value, binds))
                        })
                        .collect();
                    comp!(self, CompKind::Prim(Prim::Update { base, fields }))
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
                        CompKind::Prim(Prim::Map(
                            entries
                                .iter()
                                .map(|e| match e {
                                    MapEntry::Entry { key, value } => MapEntryV::Entry(
                                        self.spanned_val(key, binds),
                                        self.spanned_val(value, binds),
                                    ),
                                    MapEntry::Spread(a) => {
                                        MapEntryV::Spread(self.spanned_val(a, binds))
                                    }
                                })
                                .collect(),
                        ))
                    ),
                }
            }

            Ast::Tag { label, payload } => {
                let payload_val = payload.as_ref().map_or(Val::Unit, |p| {
                    self.with_span(p.span, |this| this.to_val(&p.item, binds))
                });
                comp!(
                    self,
                    CompKind::Return(Val::Variant {
                        label: label.as_str().into(),
                        payload: Box::new(payload_val),
                    })
                )
            }

            Ast::Interpolation(parts) => {
                let values: Vec<_> = parts
                    .iter()
                    .map(|part| self.with_span(part.span, |this| this.to_val(&part.item, binds)))
                    .collect();
                let parts = parts
                    .iter()
                    .zip(values)
                    .map(|(part, value)| {
                        if matches!(part.item, Ast::Literal(_)) {
                            value
                        } else {
                            let render = comp!(self, CompKind::Prim(Prim::Render(value)));
                            self.hoist(render, binds)
                        }
                    })
                    .collect();
                comp!(self, CompKind::Prim(Prim::Concat(parts)))
            }

            Ast::Binary(l, op, r) => {
                let lv = self.with_span(l.span, |this| this.to_val(&l.item, binds));
                let rv = self.with_span(r.span, |this| this.to_val(&r.item, binds));
                comp!(self, CompKind::Prim(Prim::Binary(*op, lv, rv)))
            }
            Ast::Negate(inner) => {
                let v = self.with_span(inner.span, |this| this.to_val(&inner.item, binds));
                comp!(self, CompKind::Prim(Prim::Negate(v)))
            }
            Ast::Not(inner) => {
                let v = self.with_span(inner.span, |this| this.to_val(&inner.item, binds));
                comp!(self, CompKind::Prim(Prim::Not(v)))
            }
            Ast::And(l, r) => self.lower_short_circuit(l, r, binds, Junction::And),
            Ast::Or(l, r) => self.lower_short_circuit(l, r, binds, Junction::Or),

            Ast::Index { target, keys } => {
                let mut target =
                    self.with_span(target.span, |this| this.to_val(&target.item, binds));
                let keys: Vec<_> = keys
                    .iter()
                    .map(|key| {
                        Spanned::with_span(
                            key.span,
                            self.with_span(key.span, |this| match &key.item {
                                IndexKey::Label(label) => Key::Label(label.clone().into()),
                                IndexKey::Position(n) => Key::Position(*n),
                                IndexKey::Data(ast) => Key::Data(this.to_val(ast, binds)),
                            }),
                        )
                    })
                    .collect();
                let mut keys = keys.into_iter();
                let last = keys.next_back().expect("an index has at least one key");
                for key in keys {
                    let index = comp!(self, CompKind::Prim(Prim::Index { target, key }));
                    target = self.hoist(index, binds);
                }
                comp!(self, CompKind::Prim(Prim::Index { target, key: last }))
            }

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

    fn options(&mut self, opts: &Options, binds: &mut Vec<(Pattern, Comp)>) -> Arc<FieldsNode> {
        FieldsNode::new(
            self.sorted_fields(opts.iter().map(|(key, value)| (key, value)), binds)
                .into(),
        )
    }

    /// A plain record or map literal's entries, elaborated in written order and
    /// then sorted by key, stably.
    fn sorted_fields<'a>(
        &mut self,
        items: impl IntoIterator<Item = (&'a String, &'a Spanned<Ast>)>,
        binds: &mut Vec<(Pattern, Comp)>,
    ) -> Vec<(Name, Spanned<Val>)> {
        let mut fields: Vec<(Name, _)> = items
            .into_iter()
            .map(|(key, value)| (key.as_str().into(), self.spanned_val(value, binds)))
            .collect();
        fields.sort_by(|(a, _), (b, _)| a.cmp(b));
        fields
    }
}
