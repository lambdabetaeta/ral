//! Scope forms and options.

use super::Generator;
use crate::ir::{Comp, CompKind, Fields, FieldsNode, Pattern, Prim, Val, is_synthetic};
use crate::source::{Span, Spanned, WithSpan};
use crate::ty::{CompTy, Ty};
use crate::typecheck::builtins::{Arm, audit_record, handlers_ty};
use crate::typecheck::env::Inferring;
use crate::typecheck::error::{OptionsContext, Reason, TypeErrorKind, Unwritten};
use crate::typecheck::table::{Form, Holds, Key, Table, declared};
use std::sync::Arc;

impl Unwritten {
    fn of(value: &Val) -> Self {
        match value {
            Val::Variable(name) if !is_synthetic(name) => Self::Variable(name.to_string()),
            Val::Variable(_) => Self::Computed,
            Val::Map(_) => Self::Map,
            Val::List(_) => Self::List,
            Val::Thunk(block) => {
                let mut body = block.shape().as_ref();
                let mut params = 0;
                while let CompKind::Lam { body: rest, .. } = &body.item {
                    params += 1;
                    body = rest;
                }
                Self::Block { params }
            }
            Val::Unit => Self::Unit,
            _ => Self::Value,
        }
    }
}

fn hoisted_tail(comp: &Comp) -> Option<&Comp> {
    // Synthetic Bind ... Return(same name) identifies an elaborator-hoisted tail.
    let CompKind::Bind {
        pattern,
        comp: rhs,
        rest,
        ..
    } = &comp.item
    else {
        return None;
    };
    let Pattern::Name(name) = pattern.as_ref() else {
        return None;
    };
    let CompKind::Return(Val::Variable(tail)) = &rest.item else {
        return None;
    };
    (is_synthetic(name) && tail == name).then_some(rhs.as_ref())
}

fn unwritten(comp: &Comp) -> Unwritten {
    match &comp.item {
        CompKind::Return(value) => Unwritten::of(value),
        CompKind::Prim(Prim::Update { .. }) => Unwritten::Spread,
        CompKind::Prim(Prim::Map(_)) => Unwritten::Map,
        CompKind::Prim(Prim::List(_)) => Unwritten::List,
        _ => Unwritten::Computed,
    }
}

impl Generator {
    pub(super) fn diagnose_unwritten(
        &mut self,
        table: &'static Table,
        factory: bool,
        found: Unwritten,
    ) {
        self.solver.diagnose(TypeErrorKind::OptionsNotWritten {
            form: table.form,
            found,
            offered: table.offered(),
            context: if table.factory && !factory {
                OptionsContext::FactoryBody
            } else {
                OptionsContext::File { factory }
            },
            unit_ok: !table.keys.iter().any(|key| key.required),
        });
    }

    pub(super) fn file_options(
        &mut self,
        comp: &Comp,
        table: &'static Table,
        factory: bool,
    ) -> Comp<Inferring> {
        self.with_span(comp.span, |this| {
            let hoisted = hoisted_tail(comp);
            let item = match &comp.item {
                CompKind::Bind {
                    pattern,
                    comp: rhs,
                    rest,
                    ..
                } if hoisted.is_none() => {
                    this.bind(pattern, rhs, |this| {
                        ((), this.file_options(rest, table, factory))
                    })
                    .1
                }
                CompKind::Seq { first, rest } => {
                    this.seq(first, |this| ((), this.file_options(rest, table, factory)))
                        .1
                }
                CompKind::Return(Val::Record(fields)) => {
                    CompKind::Return(Val::Record(this.check_options(fields, table)))
                }
                CompKind::Return(Val::Unit) => {
                    this.check_options(&FieldsNode::new(Fields::new()), table);
                    CompKind::Return(Val::Unit)
                }
                CompKind::Return(Val::Thunk(block)) if factory => {
                    let block = block.shape();
                    match &block.item {
                        CompKind::Lam { param, body }
                            if !matches!(body.item, CompKind::Lam { .. }) =>
                        {
                            let (_, (), item) = this.with_span(block.span, |this| {
                                this.scope(|this| {
                                    this.lam(param, |this| {
                                        ((), this.file_options(body, table, false))
                                    })
                                })
                            });
                            CompKind::Return(Val::thunk(Arc::new(Spanned::with_span(
                                block.span, item,
                            ))))
                        }
                        _ => {
                            this.diagnose_unwritten(table, factory, unwritten(comp));
                            this.statement(comp).1.item
                        }
                    }
                }
                _ => {
                    let tail = hoisted.unwrap_or(comp);
                    this.with_span(tail.span, |this| {
                        this.diagnose_unwritten(table, factory, unwritten(tail));
                    });
                    this.statement(comp).1.item
                }
            };
            Spanned::with_span(comp.span, item)
        })
    }

    pub(super) fn check_options(
        &mut self,
        opts: &FieldsNode,
        table: &'static Table,
    ) -> Arc<FieldsNode<Inferring>> {
        let mut written: Vec<_> = opts.shape().iter().collect();
        written.sort_by_key(|(_, value)| value.span.map(|span| span.start));
        let nodes = written
            .into_iter()
            .map(|(label, value)| {
                let node = self.with_span(value.span, |this| {
                    this.check_option(label, &value.item, value.span, table)
                });
                (label.clone(), Spanned::with_span(value.span, node))
            })
            .collect();
        let missing: Vec<_> = table
            .keys
            .iter()
            .filter(|key| key.required && !opts.shape().contains_key(key.label))
            .collect();
        if !missing.is_empty() {
            self.solver.diagnose(TypeErrorKind::OptionsMissing {
                form: table.form,
                missing: missing.iter().map(|key| key.label).collect(),
                blanks: missing.iter().map(|key| key.blank).collect(),
            });
        }
        FieldsNode::new(nodes)
    }

    fn check_option(
        &mut self,
        label: &str,
        value: &Val,
        at: Option<Span>,
        table: &'static Table,
    ) -> Val<Inferring> {
        let key = table.keys.iter().find(|key| key.label == label);
        if let (
            Some(Key {
                holds: Holds::Handlers,
                ..
            }),
            Val::Map(entries),
        ) = (key, value)
        {
            let nodes = entries
                .shape()
                .iter()
                .map(|(label, value)| {
                    let node = self.with_span(value.span, |this| {
                        this.install_arm(&value.item, label, value.span)
                    });
                    (label.clone(), Spanned::with_span(value.span, node))
                })
                .collect();
            return Val::Map(FieldsNode::new(nodes));
        }
        let Some(key) = key else {
            let (_, node) = self.val(value, at);
            self.solver.diagnose(TypeErrorKind::UnknownKey {
                form: table.form,
                key: label.to_string(),
                offered: table.offered(),
            });
            return node;
        };
        match key.holds {
            Holds::CatchAll => {
                let (_, node) = self.region(|this| this.arm(value, Arm::CatchAll, at));
                node
            }
            Holds::At(mint) => {
                let (found, node) = self.val(value, at);
                self.solver.equate_held(&mint(), &found, table, key);
                node
            }
            Holds::Handlers => {
                let (found, node) = self.val(value, at);
                self.solver.equate_held(&handlers_ty(), &found, table, key);
                node
            }
            Holds::Table(sub) => {
                if let Val::Record(fields) = value {
                    Val::Record(self.check_options(fields, sub))
                } else {
                    self.solver.diagnose(TypeErrorKind::TableNotWritten {
                        key: key.label,
                        first: sub.offered().first().copied(),
                        found: Unwritten::of(value),
                    });
                    self.val(value, at).1
                }
            }
            Holds::Refused(cause) => {
                let (_, node) = self.val(value, at);
                self.solver.diagnose(TypeErrorKind::RefusedKey {
                    form: table.form,
                    key: key.label,
                    advice: cause.advice(),
                });
                node
            }
            Holds::Decoded => self.val(value, at).1,
        }
    }

    pub(in crate::typecheck) fn within(
        &mut self,
        opts: &FieldsNode,
        body: &Val,
    ) -> (CompTy, CompKind<Inferring>) {
        let opts = self.check_options(opts, declared(Form::Within));
        let (cty, body) = self.scope(|this| {
            for (label, value) in opts.shape() {
                if label.as_ref() == "handlers"
                    && let Val::Map(entries) = &value.item
                {
                    for (name, _) in entries.shape() {
                        this.env.bind_handler(name.to_string());
                    }
                }
            }
            this.scope_body(body)
        });
        (cty, CompKind::Within { opts, body })
    }

    pub(in crate::typecheck) fn grant(
        &mut self,
        caps: &FieldsNode,
        body: &Val,
    ) -> (CompTy, CompKind<Inferring>) {
        let caps = self.check_options(caps, declared(Form::Grant));
        let (cty, body) = self.scope_body(body);
        (cty, CompKind::Grant { caps, body })
    }

    pub(in crate::typecheck) fn audit(&mut self, body: &Val) -> (CompTy, CompKind<Inferring>) {
        let (cty, body) = self.scope_body(body);
        let alpha = self.extract_return(&cty);
        (self.produces(audit_record(alpha)), CompKind::Audit { body })
    }

    pub(in crate::typecheck) fn spawn(
        &mut self,
        title: Option<&Spanned<Val>>,
        body: &Val,
    ) -> (CompTy, CompKind<Inferring>) {
        let title = title.map(|title| {
            self.with_span(title.span, |this| {
                let (ty, node) = this.val(&title.item, title.span);
                this.solver.equate(&ty, &Ty::string(), Reason::SpawnTitle);
                Spanned::with_span(title.span, node)
            })
        });
        let (cty, body) = self.scope_body(body);
        let producer = self.force_return_shape(&cty, Reason::ReturnShape);
        (
            self.produces(Ty::handle(producer.grade, producer.ty)),
            CompKind::Spawn { title, body },
        )
    }

    pub(in crate::typecheck) fn gather(&mut self, body: &Val) -> (CompTy, CompKind<Inferring>) {
        let (cty, body) = self.scope_body(body);
        (cty, CompKind::Gather { body })
    }

    pub(super) fn scope_body(&mut self, body: &Val) -> (CompTy, Val<Inferring>) {
        let cty = self.solver.fresh_comp();
        let (ty, node) = self.val(body, None);
        self.solver
            .equate(&ty, &Ty::thunk(cty.clone()), Reason::ScopeBody);
        self.extract_return(&cty);
        (cty, node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Argv, CommandName, CommandWord, Name, Pattern, Redirects};
    use crate::source::FileId;
    use crate::ty::{CompTyNode, Grade, Scheme, UnitId};
    use crate::typecheck::env::TyEnv;
    use crate::typecheck::error::Unwritten;
    use crate::typecheck::generate::tests::{assert_solved_type, solved};
    use crate::typecheck::readback::{Readback, exec_node, lambda_comp};
    use crate::typecheck::solve::Solver;

    static OPTIONAL: Table = Table {
        form: "custom",
        factory: false,
        keys: &[Key {
            label: "flag",
            holds: Holds::At(Ty::bool),
            required: false,
            blank: None,
            reason: None,
        }],
    };

    static REQUIRED: Table = Table {
        form: "custom",
        factory: false,
        keys: &[Key {
            label: "flag",
            holds: Holds::At(Ty::bool),
            required: true,
            blank: None,
            reason: None,
        }],
    };

    static EMPTY: Table = Table {
        form: "custom",
        factory: false,
        keys: &[],
    };

    static FACTORY: Table = Table {
        form: "custom",
        factory: true,
        keys: OPTIONAL.keys,
    };

    fn file_errors(source: &str, table: &'static Table) -> Vec<crate::TypeError> {
        match crate::compile::compile_and_typecheck(
            source,
            crate::test_helper::core_schemes(),
            FileId::DUMMY,
            None,
            crate::typecheck::Unit::Host(table),
            &mut crate::compile::no_modules,
        ) {
            Ok(_) => Vec::new(),
            Err(crate::compile::CompileError::Types(errors)) => errors,
            Err(error) => panic!("{source}: {error:?}"),
        }
    }

    fn fields(entries: Vec<(&str, Spanned<Val>)>) -> Arc<FieldsNode> {
        FieldsNode::new(
            entries
                .into_iter()
                .map(|(key, value)| (Name::from(key), value))
                .collect(),
        )
    }

    fn generator() -> Generator {
        Generator::new(Solver::new(), TyEnv::new(), Vec::new())
    }

    fn at(start: u32) -> Span {
        Span::new(FileId::DUMMY, start, start + 1)
    }

    fn node(start: u32, item: CompKind) -> Comp {
        Spanned::new(at(start), item)
    }

    fn command(start: u32, name: &str) -> Comp {
        node(
            start,
            exec_node(
                &CommandWord::Name(CommandName::Bare(name.into())),
                Argv::default(),
                Redirects::default(),
                None,
            ),
        )
    }

    fn block(comp: Comp) -> Val {
        Val::thunk(Arc::new(comp))
    }

    fn arm(params: &[&str], body: Comp) -> Val {
        block(lambda_comp(
            body.span,
            params.iter().map(|name| (*name).into()).collect(),
            body.item,
        ))
    }

    #[test]
    fn within_grant_guard_and_gather_preserve_the_body_producer() {
        for output in [false, true] {
            for form in 0..4 {
                let mut generator = generator();
                let comp = if output {
                    command(1, "emit")
                } else {
                    node(1, CompKind::Return(Val::Int(1)))
                };
                let guard_body = arm(&["resource"], comp.clone());
                let body = block(comp);
                let opts = FieldsNode::new(Fields::new());
                let (cty, item) = match form {
                    0 => generator.within(&opts, &body),
                    1 => generator.grant(&opts, &body),
                    2 => generator.guard(
                        &block(node(0, CompKind::Return(Val::Unit))),
                        &guard_body,
                        &arm(&["resource", "end"], command(2, "release")),
                    ),
                    _ => generator.gather(&body),
                };
                let solution = solved(generator.solver);
                assert_eq!(
                    solution.resolve_comp(&cty),
                    if output {
                        CompTy::command()
                    } else {
                        CompTy::pure(Ty::int())
                    }
                );
                Readback::new(&solution, UnitId::fresh(), 0).comp(&Spanned::new(at(0), item));
            }
        }
    }

    #[test]
    fn scope_bodies_require_ready_thunks_and_guard_release_is_checked_independently() {
        let mut generator = generator();
        generator.gather(&Val::Int(1));
        generator.guard(
            &block(node(1, CompKind::Return(Val::Int(1)))),
            &arm(&["resource"], node(2, CompKind::Return(Val::Int(1)))),
            &arm(&["resource"], node(3, CompKind::Return(Val::Unit))),
        );
        let errors = generator.solver.refused().1;
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(matches!(errors[0].reason, Some(Reason::ScopeBody)));
        assert!(matches!(errors[1].reason, Some(Reason::GuardRelease)));
    }

    #[test]
    fn audit_keeps_the_raw_output_unit_instead_of_capturing_text() {
        let mut generator = generator();
        let (cty, item) = generator.audit(&block(command(1, "emit")));
        assert!(matches!(item, CompKind::Audit { .. }));
        let solution = solved(generator.solver);
        let resolved = solution.resolve_comp(&cty);
        let CompTyNode::Return(grade, result) = resolved.node() else {
            panic!("audit producer")
        };
        assert!(matches!(grade, Grade::Var(_)));
        assert_solved_type(&solution.scratch(), result, &audit_record(Ty::unit()));
    }

    #[test]
    fn spawn_preserves_the_child_grade_in_its_handle_and_title_span() {
        let mut generator = generator();
        let (cty, item) = generator.spawn(
            Some(&Spanned::new(at(1), Val::String("child".into()))),
            &block(command(2, "emit")),
        );
        let CompKind::Spawn {
            title: Some(title), ..
        } = item
        else {
            panic!("spawn title preserved")
        };
        assert_eq!(title.span, Some(at(1)));
        let solution = solved(generator.solver);
        let resolved = solution.resolve_comp(&cty);
        let CompTyNode::Return(grade, result) = resolved.node() else {
            panic!("spawn producer")
        };
        assert!(matches!(grade, Grade::Var(_)));
        assert_solved_type(
            &solution.scratch(),
            result,
            &Ty::handle(Grade::Output, Ty::unit()),
        );

        let mut generator = self::generator();
        generator.spawn(
            Some(&Spanned::new(at(4), Val::Int(1))),
            &block(command(5, "emit")),
        );
        let errors = generator.solver.refused().1;
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].pos, Some(at(4)));
        assert!(matches!(errors[0].reason, Some(Reason::SpawnTitle)));
    }

    #[test]
    fn options_check_in_source_order_but_keep_their_original_field_order() {
        static TABLE: Table = Table {
            form: "test scope",
            keys: &[
                Key {
                    label: "later",
                    holds: Holds::At(Ty::string),
                    required: false,
                    blank: None,
                    reason: None,
                },
                Key {
                    label: "early",
                    holds: Holds::At(Ty::bool),
                    required: false,
                    blank: None,
                    reason: None,
                },
                Key {
                    label: "required",
                    holds: Holds::At(Ty::string),
                    required: true,
                    blank: None,
                    reason: None,
                },
            ],
            factory: false,
        };
        let mut generator = generator();
        let opts = fields(vec![
            ("later", Spanned::new(at(5), Val::Int(1))),
            ("early", Spanned::new(at(2), Val::Int(2))),
        ]);
        let nodes = generator.check_options(&opts, &TABLE);
        assert_eq!(nodes.shape()["later"].span, Some(at(5)));
        assert_eq!(nodes.shape()["early"].span, Some(at(2)));
        let errors = generator.solver.refused().1;
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors.iter().any(|error| matches!(
            &error.kind, TypeErrorKind::OptionsMissing { missing, .. } if missing == &["required"]
        )));
        let positions: Vec<_> = errors.iter().filter_map(|error| error.pos).collect();
        assert_eq!(positions, [at(2), at(5)]);
    }

    #[test]
    fn grant_options_keep_unknown_refused_and_vocabulary_refusals_distinct() {
        let mut generator = generator();
        let caps = fields(vec![
            ("missing", Spanned::new(at(1), Val::Unit)),
            ("detach", Spanned::new(at(2), Val::Bool(true))),
            ("net", Spanned::new(at(3), Val::Int(1))),
        ]);
        generator.grant(&caps, &block(command(4, "emit")));
        let errors = generator.solver.refused().1;
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(matches!(errors[0].kind, TypeErrorKind::UnknownKey { .. }));
        assert!(matches!(
            errors[1].kind,
            TypeErrorKind::RefusedKey { key: "detach", .. }
        ));
        assert!(matches!(errors[2].reason, Some(Reason::Vocabulary(_))));
        assert_eq!(errors[2].pos, Some(at(3)));
    }

    #[test]
    fn written_nested_tables_constrain_their_values_at_the_form() {
        let mut generator = generator();
        let unknown = generator.solver.fresh_ty();
        generator
            .env
            .bind("targets".into(), Scheme::mono(unknown.clone()));
        let caps = fields(vec![(
            "exec",
            Spanned::new(
                at(1),
                Val::record(vec![(
                    "allow".into(),
                    Spanned::new(at(2), Val::Variable("targets".into())),
                )]),
            ),
        )]);
        generator.grant(&caps, &block(command(3, "emit")));
        assert!(
            !generator
                .solver
                .equate(&unknown, &Ty::string(), Reason::Argument)
        );
        let (store, errors) = generator.solver.refused();
        let [error] = &errors[..] else {
            panic!("{errors:?}")
        };
        assert!(matches!(error.kind, TypeErrorKind::TyMismatch { .. }));
        assert_eq!(
            store.resolve_ty(&unknown),
            Ty::list(crate::typecheck::table::EXEC_TARGET.ty())
        );
    }

    #[test]
    fn bound_nested_tables_are_refused_without_guessing_their_contents() {
        let mut generator = generator();
        let unknown = generator.solver.fresh_ty();
        generator
            .env
            .bind("caps".into(), Scheme::mono(unknown.clone()));
        let caps = fields(vec![(
            "exec",
            Spanned::new(at(2), Val::Variable("caps".into())),
        )]);
        generator.grant(&caps, &block(command(3, "emit")));
        assert!(
            generator
                .solver
                .equate(&unknown, &Ty::string(), Reason::Argument)
        );
        let (store, errors) = generator.solver.refused();
        let [error] = &errors[..] else {
            panic!("{errors:?}")
        };
        assert!(matches!(&error.kind, TypeErrorKind::TableNotWritten {
            key: "exec", found: Unwritten::Variable(name), ..
        } if name == "caps"));
        assert_eq!(error.pos, Some(at(2)));
        assert_eq!(store.resolve_ty(&unknown), Ty::string());
    }

    #[test]
    fn within_installs_named_arms_only_for_its_body_and_keeps_arm_spans() {
        let mut generator = generator();
        generator.env.bind_handler("outer".into());
        let entries = fields(vec![(
            "tool",
            Spanned::new(
                at(1),
                block(node(
                    2,
                    CompKind::Lam {
                        param: "args".into(),
                        body: Arc::new(command(3, "tool")),
                    },
                )),
            ),
        )]);
        let opts = fields(vec![("handlers", Spanned::new(at(0), Val::Map(entries)))]);
        let (cty, item) = generator.within(&opts, &block(command(4, "tool")));
        let CompKind::Within { opts, .. } = item else {
            panic!("within preserved")
        };
        let Val::Map(entries) = &opts.shape()["handlers"].item else {
            panic!("handler map preserved")
        };
        assert_eq!(entries.shape()["tool"].span, Some(at(1)));
        assert!(generator.env.is_handled("outer"));
        assert!(!generator.env.is_handled("tool"));
        assert!(generator.env.lookup_binding("args").is_none());
        let solution = solved(generator.solver);
        assert_eq!(solution.resolve_comp(&cty), CompTy::command());
    }

    #[test]
    fn written_handler_maps_bind_labels_for_body_diagnostics_but_bound_maps_do_not() {
        for (source, handled) in [
            (
                "within [handlers: [:, tool: { |args| echo ...$args }]] { tool () }",
                true,
            ),
            (
                "let h = [:, tool: { |args| echo ...$args }]; within [handlers: $h] { tool () }",
                false,
            ),
        ] {
            let errors = crate::compile::compile_and_typecheck(
                source,
                crate::test_helper::core_schemes(),
                FileId::DUMMY,
                None,
                crate::typecheck::Unit::Script,
                &mut crate::compile::no_modules,
            )
            .expect_err("unit is not a command argument");
            let crate::compile::CompileError::Types(errors) = errors else {
                panic!("{source}: {errors:?}")
            };
            let [error] = &errors[..] else {
                panic!("one argv refusal: {errors:?}")
            };
            assert!(matches!(
                error.kind,
                TypeErrorKind::ExecArgNotText { handled: found, .. } if found == handled
            ));
        }
    }

    #[test]
    fn written_handler_maps_check_each_arm_without_joining_members() {
        let mut generator = generator();
        let entries = fields(vec![
            (
                "first",
                Spanned::new(
                    at(1),
                    arm(&["args"], node(2, CompKind::Return(Val::Int(1)))),
                ),
            ),
            (
                "second",
                Spanned::new(
                    at(3),
                    arm(&["args"], node(4, CompKind::Return(Val::Bool(true)))),
                ),
            ),
        ]);
        generator.check_option(
            "handlers",
            &Val::Map(entries),
            Some(at(0)),
            declared(Form::Within),
        );
        let errors = generator.solver.refused().1;
        assert_eq!(errors.len(), 2, "{errors:?}");
        for (error, name, pos) in [(&errors[0], "first", at(1)), (&errors[1], "second", at(3))] {
            assert!(matches!(
                &error.reason,
                Some(Reason::StandsIn(crate::typecheck::Standing::Command(found))) if found == name
            ));
            assert_eq!(error.pos, Some(pos));
        }
    }

    #[test]
    fn unwritten_classification_is_syntactic_and_never_prints_synthetic_names() {
        use crate::ir::{Prim, synthetic};
        for (value, found) in [
            (
                Val::Variable("cfg".into()),
                Unwritten::Variable("cfg".into()),
            ),
            (Val::Variable(synthetic("tail", 0)), Unwritten::Computed),
            (Val::map(vec![]), Unwritten::Map),
            (Val::list(vec![]), Unwritten::List),
            (
                arm(&[], node(1, CompKind::Return(Val::Unit))),
                Unwritten::Block { params: 0 },
            ),
            (
                arm(&["a"], node(1, CompKind::Return(Val::Unit))),
                Unwritten::Block { params: 1 },
            ),
            (
                arm(&["a", "b"], node(1, CompKind::Return(Val::Unit))),
                Unwritten::Block { params: 2 },
            ),
            (
                block(node(
                    1,
                    CompKind::Return(arm(&["inner"], node(2, CompKind::Return(Val::Unit)))),
                )),
                Unwritten::Block { params: 0 },
            ),
            (
                arm(
                    &["outer"],
                    node(
                        1,
                        CompKind::Return(arm(&["inner"], node(2, CompKind::Return(Val::Unit)))),
                    ),
                ),
                Unwritten::Block { params: 1 },
            ),
            (Val::Unit, Unwritten::Unit),
            (Val::Int(1), Unwritten::Value),
            (
                Val::Float(crate::data::Finite::new(1.0).expect("finite")),
                Unwritten::Value,
            ),
            (Val::Bool(true), Unwritten::Value),
            (Val::String("x".into()), Unwritten::Value),
            (Val::record(vec![]), Unwritten::Value),
            (
                Val::Variant {
                    label: "tag".into(),
                    payload: Box::new(Val::Unit),
                },
                Unwritten::Value,
            ),
        ] {
            assert_eq!(Unwritten::of(&value), found);
            assert_eq!(unwritten(&node(0, CompKind::Return(value))), found);
        }
        for (comp, found) in [
            (
                CompKind::Prim(Prim::Update {
                    base: Spanned::new(at(1), Val::record(vec![])),
                    fields: Box::new([]),
                }),
                Unwritten::Spread,
            ),
            (CompKind::Prim(Prim::Map(Box::new([]))), Unwritten::Map),
            (CompKind::Prim(Prim::List(Box::new([]))), Unwritten::List),
            (CompKind::Force(Val::Unit), Unwritten::Computed),
        ] {
            assert_eq!(unwritten(&node(0, comp)), found);
        }
    }

    #[test]
    fn hoisted_tails_require_a_synthetic_name_returned_unchanged() {
        let name = crate::ir::synthetic("tail", 0);
        for (pattern, value, expected) in [
            (
                Pattern::Name(name.clone()),
                Val::Variable(name.clone()),
                true,
            ),
            (
                Pattern::Name("tail".into()),
                Val::Variable("tail".into()),
                false,
            ),
            (
                Pattern::Name(name.clone()),
                Val::Variable("other".into()),
                false,
            ),
            (Pattern::Name(name), Val::Unit, false),
            (Pattern::Wildcard, Val::Unit, false),
        ] {
            let comp = node(
                0,
                CompKind::bind(
                    pattern,
                    node(1, CompKind::Return(Val::Unit)),
                    node(2, CompKind::Return(value)),
                ),
            );
            let rhs = hoisted_tail(&comp);
            assert_eq!(rhs.is_some(), expected);
            if let Some(rhs) = rhs {
                let CompKind::Bind { comp, .. } = &comp.item else {
                    panic!("constructed bind")
                };
                assert!(std::ptr::eq(rhs, comp.as_ref()));
            }
        }
        assert!(hoisted_tail(&node(0, CompKind::Return(Val::Unit))).is_none());
    }

    #[test]
    fn rejected_tails_still_check_their_computations() {
        for (comp, table) in [
            (node(1, CompKind::Force(Val::Int(1))), &OPTIONAL),
            (
                node(
                    0,
                    CompKind::Return(block(node(1, CompKind::Force(Val::Int(1))))),
                ),
                &FACTORY,
            ),
        ] {
            let mut generator = generator();
            generator.file_options(&comp, table, table.factory);
            let errors = generator.solver.refused().1;
            assert_eq!(errors.len(), 2, "{errors:?}");
            assert!(matches!(
                errors[0].kind,
                TypeErrorKind::OptionsNotWritten { .. }
            ));
            assert!(matches!(errors[1].reason, Some(Reason::ForceOperand)));
            assert_eq!(errors[1].pos, Some(at(1)));
        }
    }

    #[test]
    fn file_options_accept_written_records_unit_and_empty_files() {
        for source in [
            "[flag: true]",
            "return [flag: true]",
            "let value = true; return [flag: $value]",
            "let value = 7; echo preparing; let value = true; return [flag: $value]",
            "let value = true; let next = $value; return [flag: $next]",
            "return [flag: !{ return true }]",
            "()",
            "return ()",
            "return",
            "",
        ] {
            assert!(file_errors(source, &OPTIONAL).is_empty(), "{source}");
        }
        for source in ["()", "return ()", "return", ""] {
            let errors = file_errors(source, &REQUIRED);
            let [error] = &errors[..] else {
                panic!("{source}: {errors:?}")
            };
            assert!(
                matches!(&error.kind, TypeErrorKind::OptionsMissing { missing, .. } if missing == &["flag"])
            );
        }
    }

    #[test]
    fn file_options_rebuild_the_written_value_and_keep_its_spans() {
        for value in [
            Val::Unit,
            Val::record(vec![("flag".into(), Spanned::new(at(2), Val::Bool(true)))]),
        ] {
            let mut generator = generator();
            let comp =
                generator.file_options(&node(1, CompKind::Return(value.clone())), &OPTIONAL, false);
            assert_eq!(comp.span, Some(at(1)));
            if let CompKind::Return(Val::Record(fields)) = &comp.item {
                assert_eq!(fields.shape()["flag"].span, Some(at(2)));
                assert!(matches!(fields.shape()["flag"].item, Val::Bool(true)));
            }
            let solution = solved(generator.solver);
            let checked = Readback::new(&solution, UnitId::fresh(), 0).comp(&comp);
            assert_eq!(checked.span, Some(at(1)));
            assert_eq!(checked.item, CompKind::Return(value));
        }
        let mut generator = generator();
        let body = node(
            4,
            CompKind::Lam {
                param: "options".into(),
                body: Arc::new(node(9, CompKind::Return(Val::Unit))),
            },
        );
        let comp = generator.file_options(&node(0, CompKind::Return(block(body))), &FACTORY, true);
        let CompKind::Return(Val::Thunk(body)) = &comp.item else {
            panic!("written factory")
        };
        assert_eq!(comp.span, Some(at(0)));
        assert_eq!(body.shape().span, Some(at(4)));
        let CompKind::Lam { body, .. } = &body.shape().item else {
            panic!("factory lambda")
        };
        assert_eq!(body.span, Some(at(9)));
        let _ = solved(generator.solver);
    }

    #[test]
    fn file_options_refuse_each_unwritten_tail_shape() {
        for (source, found) in [
            (
                "let cfg = [flag: true]; return $cfg",
                Unwritten::Variable("cfg".into()),
            ),
            ("return !{ return [flag: true] }", Unwritten::Computed),
            ("return [:]", Unwritten::Map),
            ("let key = 'flag'; return [$key: true]", Unwritten::Map),
            ("return []", Unwritten::List),
            ("let xs = []; return [...$xs]", Unwritten::List),
            ("return { [flag: true] }", Unwritten::Block { params: 0 }),
            (
                "return { |a| [flag: true] }",
                Unwritten::Block { params: 1 },
            ),
            (
                "return { |a b| [flag: true] }",
                Unwritten::Block { params: 2 },
            ),
            ("let value = 1", Unwritten::Let),
            ("return 7", Unwritten::Value),
        ] {
            let errors = file_errors(source, &OPTIONAL);
            let [error] = &errors[..] else {
                panic!("{source}: {errors:?}")
            };
            assert!(
                matches!(&error.kind, TypeErrorKind::OptionsNotWritten {
                found: actual, context: OptionsContext::File { factory: false }, unit_ok: true, ..
            } if actual == &found),
                "{source}: {error:?}"
            );
        }
    }

    #[test]
    fn both_bare_and_returned_spreads_are_classified_at_the_spread() {
        for tail in ["[...$cfg, flag: false]", "return [...$cfg, flag: false]"] {
            let source = format!("let cfg = [flag: true]; {tail}");
            let errors = file_errors(&source, &OPTIONAL);
            let [error] = &errors[..] else {
                panic!("{source}: {errors:?}")
            };
            assert!(matches!(
                error.kind,
                TypeErrorKind::OptionsNotWritten {
                    found: Unwritten::Spread,
                    ..
                }
            ));
            let pos = error.pos.expect("spread tail");
            assert_eq!(
                &source[pos.start as usize..pos.end as usize],
                "[...$cfg, flag: false]"
            );
            assert_eq!(
                error.kind.render_label(),
                "a spread hides which keys it brings"
            );
        }
    }

    #[test]
    fn computed_tail_diagnostics_exclude_return() {
        for tail in [
            "!{ return [flag: true] }",
            "return !{ return [flag: true] }",
        ] {
            let source = format!("echo preparing; {tail}");
            let errors = file_errors(&source, &OPTIONAL);
            let [error] = &errors[..] else {
                panic!("{source}: {errors:?}")
            };
            assert!(matches!(
                error.kind,
                TypeErrorKind::OptionsNotWritten {
                    found: Unwritten::Computed,
                    ..
                }
            ));
            let pos = error.pos.expect("computed tail");
            assert_eq!(
                &source[pos.start as usize..pos.end as usize],
                "!{ return [flag: true] }"
            );
        }
    }

    #[test]
    fn rejected_hoisted_tails_keep_full_node_spans_but_diagnose_the_rhs() {
        let mut generator = generator();
        let name = crate::ir::synthetic("tail", 0);
        let source = node(
            0,
            CompKind::bind(
                Pattern::Name(name.clone()),
                node(
                    3,
                    CompKind::Force(block(node(4, CompKind::Return(Val::Unit)))),
                ),
                node(9, CompKind::Return(Val::Variable(name))),
            ),
        );
        let comp = generator.file_options(&source, &OPTIONAL, false);
        assert_eq!(comp.span, Some(at(0)));
        let CompKind::Bind {
            comp: rhs, rest, ..
        } = &comp.item
        else {
            panic!("hoisted bind preserved")
        };
        assert_eq!(rhs.span, Some(at(3)));
        assert_eq!(rest.span, Some(at(9)));
        let errors = generator.solver.refused().1;
        let [error] = &errors[..] else {
            panic!("{errors:?}")
        };
        assert!(matches!(
            error.kind,
            TypeErrorKind::OptionsNotWritten {
                found: Unwritten::Computed,
                ..
            }
        ));
        assert_eq!(error.pos, Some(at(3)));
    }

    #[test]
    fn nested_spreads_are_computed_and_never_name_the_hoisted_binder() {
        for source in [
            "let cfg = [allow: [], deny: []]; grant [exec: [...$cfg, deny: []]] { echo ok }; return ()",
            "let cfg = [allow: [], deny: []]; return [exec: [...$cfg, deny: []]]",
        ] {
            let errors = file_errors(source, declared(Form::Grant));
            let [error] = &errors[..] else {
                panic!("{source}: {errors:?}")
            };
            assert!(matches!(
                error.kind,
                TypeErrorKind::TableNotWritten {
                    key: "exec",
                    found: Unwritten::Computed,
                    ..
                }
            ));
            assert_eq!(error.kind.render_label(), "a computed value hides its keys");
            for text in [
                error.message(),
                error.kind.render_label(),
                error.hint().expect("rewrite"),
            ] {
                assert!(!text.contains('%'), "{text}");
            }
            let pos = error.pos.expect("nested value");
            assert_eq!(
                &source[pos.start as usize..pos.end as usize],
                "[...$cfg, deny: []]"
            );
        }
    }

    #[test]
    fn factories_admit_one_written_parameter_level_and_check_the_body_tail() {
        for source in [
            "return { |options| [flag: true] }",
            "{ |options| let value = true; return [flag: $value] }",
            "{ |options| [flag: $options[flag]] }",
            "{ |options| [flag: !{ return true }] }",
            "{ |options| echo preparing; [flag: true] }",
            "{ |options| () }",
            "{ |options| }",
            "{ |options| let value = true }",
        ] {
            assert!(file_errors(source, &FACTORY).is_empty(), "{source}");
        }
        for (source, found, factory) in [
            ("{ [flag: true] }", Unwritten::Block { params: 0 }, true),
            (
                "{ |a b| [flag: true] }",
                Unwritten::Block { params: 2 },
                true,
            ),
            (
                "{ |options| { |inner| [flag: true] } }",
                Unwritten::Block { params: 2 },
                true,
            ),
            (
                "{ |options| return { |inner| [flag: true] } }",
                Unwritten::Block { params: 2 },
                true,
            ),
            (
                "{ |options| echo preparing; return { |inner| [flag: true] } }",
                Unwritten::Block { params: 1 },
                false,
            ),
            (
                "{ |options| let cfg = [flag: true]; return $cfg }",
                Unwritten::Variable("cfg".into()),
                false,
            ),
            (
                "{ |options| return !{ [flag: true] } }",
                Unwritten::Computed,
                false,
            ),
            (
                "let factory = { |options| [flag: true] }; return $factory",
                Unwritten::Variable("factory".into()),
                true,
            ),
        ] {
            let errors = file_errors(source, &FACTORY);
            let [error] = &errors[..] else {
                panic!("{source}: {errors:?}")
            };
            assert!(
                matches!(&error.kind, TypeErrorKind::OptionsNotWritten {
                found: actual, context, ..
            } if actual == &found && matches!((context, factory),
                (OptionsContext::File { factory: true }, true)
                    | (OptionsContext::FactoryBody, false))),
                "{source}: {error:?}"
            );
        }
        for source in ["{ |options| }", "{ |options| let value = true }"] {
            let errors = file_errors(source, declared(Form::Manifest));
            let [error] = &errors[..] else {
                panic!("{source}: {errors:?}")
            };
            assert!(
                matches!(&error.kind, TypeErrorKind::OptionsMissing { missing, .. } if missing == &["name"])
            );
        }
    }

    #[test]
    fn factory_body_hints_rewrite_the_body_not_the_file() {
        for (source, tail) in [
            ("{ |options| return 7 }", "7"),
            ("{ |options| return [] }", "[]"),
            (
                "{ |options| echo preparing; return { |inner| [flag: true] } }",
                "{ |inner| [flag: true] }",
            ),
        ] {
            let errors = file_errors(source, &FACTORY);
            let [error] = &errors[..] else {
                panic!("{source}: {errors:?}")
            };
            assert!(matches!(
                error.kind,
                TypeErrorKind::OptionsNotWritten {
                    context: OptionsContext::FactoryBody,
                    ..
                }
            ));
            assert!(error.message().contains("factory body"));
            let hint = error.hint().expect("body rewrite");
            assert!(hint.contains("factory body"), "{source}: {hint}");
            assert!(!hint.contains("file"), "{source}: {hint}");
            assert!(!hint.contains("return { |options|"), "{source}: {hint}");
            let pos = error.pos.expect("body tail");
            assert_eq!(&source[pos.start as usize..pos.end as usize], tail);
        }
    }

    #[test]
    fn file_options_preserve_exact_tail_spans_through_bind_seq_and_factory() {
        for prefix in 0..3 {
            let mut generator = generator();
            generator.env.bind("cfg".into(), Scheme::mono(Ty::unit()));
            let tail = node(9, CompKind::Return(Val::Variable("cfg".into())));
            let comp = match prefix {
                0 => node(
                    0,
                    CompKind::bind(
                        Pattern::Name("value".into()),
                        node(1, CompKind::Return(Val::Int(1))),
                        tail,
                    ),
                ),
                1 => node(
                    0,
                    CompKind::Seq {
                        first: Arc::new(command(1, "echo")),
                        rest: Arc::new(tail),
                    },
                ),
                _ => node(0, CompKind::Return(arm(&["options"], tail))),
            };
            generator.file_options(&comp, &FACTORY, true);
            let errors = generator.solver.refused().1;
            let [error] = &errors[..] else {
                panic!("{errors:?}")
            };
            assert!(
                matches!(&error.kind, TypeErrorKind::OptionsNotWritten { found: Unwritten::Variable(name), .. } if name == "cfg")
            );
            assert_eq!(error.pos, Some(at(9)));
        }
    }

    #[test]
    fn missing_required_keys_are_located_at_the_terminal_record() {
        for source in [
            "echo preparing; return [other: true]",
            "return { |options| echo preparing; [other: true] }",
        ] {
            static TABLE: Table = Table {
                form: "custom",
                factory: true,
                keys: &[
                    Key {
                        label: "flag",
                        holds: Holds::At(Ty::bool),
                        required: true,
                        blank: None,
                        reason: None,
                    },
                    Key {
                        label: "other",
                        holds: Holds::At(Ty::bool),
                        required: false,
                        blank: None,
                        reason: None,
                    },
                ],
            };
            let errors = file_errors(source, &TABLE);
            let [error] = &errors[..] else {
                panic!("{source}: {errors:?}")
            };
            assert!(
                matches!(&error.kind, TypeErrorKind::OptionsMissing { missing, .. } if missing == &["flag"])
            );
            let pos = error.pos.expect("terminal record");
            assert_eq!(
                &source[pos.start as usize..pos.end as usize],
                "[other: true]"
            );
        }
    }

    #[test]
    fn written_options_refuse_duplicate_keys_at_the_second_value() {
        for (source, table, value) in [
            ("return [flag: true, flag: false]", &OPTIONAL, "false"),
            (
                "return { |options| [flag: true, flag: false] }",
                &FACTORY,
                "false",
            ),
            (
                "return [exec: [allow: [], allow: []]]",
                declared(Form::Grant),
                "[]",
            ),
        ] {
            let errors = file_errors(source, table);
            let [error] = &errors[..] else {
                panic!("{source}: {errors:?}")
            };
            assert!(matches!(error.kind, TypeErrorKind::DuplicateField { .. }));
            let pos = error.pos.expect("second value");
            assert_eq!(pos.start as usize, source.rfind(value).unwrap());
            assert_eq!(&source[pos.start as usize..pos.end as usize], value);
        }
    }

    #[test]
    fn custom_table_wording_respects_requiredness_and_empty_offerings() {
        for (table, record, unit_ok) in [
            (&REQUIRED, "[flag: …]", false),
            (&OPTIONAL, "[flag: …]", true),
            (&EMPTY, "[…]", true),
        ] {
            for (source, found, advice) in [
                (
                    "return []",
                    Unwritten::List,
                    "`[]` is the empty list, not a record".to_owned(),
                ),
                (
                    "return 7",
                    Unwritten::Value,
                    format!("end the file in `return {record}`"),
                ),
                (
                    "let value = 1",
                    Unwritten::Let,
                    format!("end the file in `return {record}`"),
                ),
            ] {
                let errors = file_errors(source, table);
                let [error] = &errors[..] else {
                    panic!("{source}: {errors:?}")
                };
                assert!(matches!(&error.kind, TypeErrorKind::OptionsNotWritten {
                    found: actual, unit_ok: actual_unit, ..
                } if actual == &found && *actual_unit == unit_ok));
                assert_eq!(
                    error.message(),
                    format!(
                        "this file is read as `custom`'s options, so it must end in them written out, `return {record}`"
                    )
                );
                let unit = if !unit_ok {
                    ""
                } else if found == Unwritten::List {
                    "; a file with nothing to set ends in `return ()`"
                } else {
                    ", or `return ()` to set nothing"
                };
                assert_eq!(error.hint(), Some(format!("{advice}{unit}")));
            }
        }
    }
}
