//! Scope forms, options and retained protection contracts.

use super::Generator;
use super::call::Arm;
use crate::ir::{Comp, CompKind, FieldsNode, HandlerArmV, Val};
use crate::source::{Span, Spanned, WithSpan};
use crate::ty::{CompTy, Scheme, Site, Ty};
use crate::typecheck::builtins::audit_record;
use crate::typecheck::contract::{Form, Held, Holds, Key, NestedCheck, Table, declared};
use crate::typecheck::env::{HandlerOrigin, Inferring};
use crate::typecheck::error::{Reason, TypeErrorKind};
use std::sync::Arc;

type HandlerBindings = (Vec<(String, Scheme)>, Option<Vec<HandlerArmV<Inferring>>>);

impl Generator {
    fn check_options(
        &mut self,
        opts: &FieldsNode,
        table: &'static Table,
    ) -> Arc<FieldsNode<Inferring>> {
        let mut written: Vec<_> = opts.shape().iter().enumerate().collect();
        written.sort_by_key(|(_, (_, value))| value.span.map(|span| span.start));
        let mut nodes: Vec<_> = (0..written.len()).map(|_| None).collect();
        for (i, (label, value)) in written {
            let node = self.with_span(value.span, |this| {
                this.check_option(label, &value.item, value.span, table)
            });
            nodes[i] = Some((label.clone(), Spanned::with_span(value.span, node)));
        }
        for key in table.keys {
            if matches!(key.holds, Holds::Required(_))
                && !opts
                    .shape()
                    .iter()
                    .any(|(label, _)| label.as_ref() == key.label)
            {
                self.solver.diagnose(TypeErrorKind::RowMissingField {
                    label: key.label.to_string(),
                });
            }
        }
        FieldsNode::new(
            nodes
                .into_iter()
                .map(|node| node.expect("option generated"))
                .collect(),
        )
    }

    fn check_option(
        &mut self,
        label: &str,
        value: &Val,
        at: Option<Span>,
        table: &'static Table,
    ) -> Val<Inferring> {
        let key = table.keys.iter().find(|key| key.label == label);
        let (found, node) = match key {
            Some(Key {
                holds: Holds::CatchAll,
                ..
            }) => {
                let (cty, node) = self.region(|this| this.arm(value, Arm::CatchAll, at));
                (Ty::thunk(cty), node)
            }
            _ => self.val(value, at),
        };
        let Some(key) = key else {
            self.solver.diagnose(TypeErrorKind::UnknownKey {
                form: table.form,
                key: label.to_string(),
                offered: table.offered(),
            });
            return node;
        };
        match key.holds.held() {
            Held::Unify(declared) => {
                self.solver.equate_held(&declared, &found, table, key);
            }
            Held::Nested(sub) => {
                let pos = *self.span_slot();
                self.solver.nested(NestedCheck {
                    key: key.label,
                    table: sub,
                    ty: found,
                    pos,
                });
            }
            Held::Refuse(advice) => self.solver.diagnose(TypeErrorKind::RefusedKey {
                form: table.form,
                key: key.label,
                advice,
            }),
            Held::Nothing => {}
        }
        node
    }

    fn handler_bindings(&mut self, arms: Option<&[HandlerArmV]>) -> HandlerBindings {
        let mut bindings = Vec::new();
        let nodes = arms.map(|arms| {
            arms.iter()
                .map(|arm| {
                    let (scheme, value) = self.with_span(arm.value.span, |this| {
                        this.install_arm(&arm.value.item, &arm.name, arm.value.span)
                    });
                    bindings.push((arm.name.clone(), scheme));
                    HandlerArmV {
                        name: arm.name.clone(),
                        value: Spanned::with_span(arm.value.span, value),
                    }
                })
                .collect()
        });
        (bindings, nodes)
    }

    pub(in crate::typecheck) fn within(
        &mut self,
        opts: &FieldsNode,
        handlers: Option<&[HandlerArmV]>,
        body: &Val,
    ) -> (CompTy, CompKind<Inferring>) {
        let opts = self.check_options(opts, declared(Form::Within));
        let (bindings, handlers) = self.handler_bindings(handlers);
        let (cty, body) = self.scope(|this| {
            for (name, scheme) in bindings {
                this.env.bind_handler(name, scheme, HandlerOrigin::Within);
            }
            this.scope_body(body)
        });
        (
            cty,
            CompKind::Within {
                opts,
                handlers,
                body,
            },
        )
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

    pub(in crate::typecheck) fn guard(
        &mut self,
        body: &Val,
        cleanup: &Val,
    ) -> (CompTy, CompKind<Inferring>) {
        let (cty, body) = self.scope_body(body);
        let (_, cleanup) = self.scope_body(cleanup);
        (cty, CompKind::Guard { body, cleanup })
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

    fn scope_body(&mut self, body: &Val) -> (CompTy, Val<Inferring>) {
        let cty = self.solver.fresh_comp();
        let (ty, node) = self.val(body, None);
        self.solver
            .equate(&ty, &Ty::thunk(cty.clone()), Reason::ScopeBody);
        self.extract_return(&cty);
        (cty, node)
    }

    pub(in crate::typecheck) fn protected_comp(
        &mut self,
        body: &Comp,
        site: &Arc<Site>,
    ) -> (CompTy, CompKind<Inferring>) {
        let (have, body) = self.comp(body);
        let result = self.force_return_shape(&have, Reason::ProtectContract).ty;
        let want = self.solver.contract(site);
        self.solver.equate(&result, &want, Reason::ProtectContract);
        (
            have,
            CompKind::Protect {
                body: Arc::new(body),
                site: Arc::clone(site),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Argv, CommandName, CommandWord, Pattern, Redirects};
    use crate::source::FileId;
    use crate::ty::{CompTyNode, Grade, UnitId};
    use crate::typecheck::env::TyEnv;
    use crate::typecheck::generate::tests::{assert_solved_comp, assert_solved_type, solved};
    use crate::typecheck::readback::{Readback, exec_node};
    use crate::typecheck::solve::{Mode, Solver};

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

    #[test]
    fn within_grant_guard_and_gather_preserve_the_body_producer() {
        for output in [false, true] {
            for form in 0..4 {
                let mut generator = generator();
                let body = block(if output {
                    command(1, "emit")
                } else {
                    node(1, CompKind::Return(Val::Int(1)))
                });
                let opts = FieldsNode::new(Vec::new().into());
                let (cty, item) = match form {
                    0 => generator.within(&opts, None, &body),
                    1 => generator.grant(&opts, &body),
                    2 => generator.guard(&body, &block(command(2, "cleanup"))),
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
    fn scope_bodies_require_ready_thunks_and_cleanup_is_checked_independently() {
        let mut generator = generator();
        generator.gather(&Val::Int(1));
        generator.guard(
            &block(node(1, CompKind::Return(Val::Int(1)))),
            &block(node(
                2,
                CompKind::Lam {
                    param: "x".into(),
                    body: Arc::new(node(3, CompKind::Return(Val::Unit))),
                },
            )),
        );
        let errors = generator.solver.refused().1;
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(matches!(errors[0].reason, Some(Reason::ScopeBody)));
        assert!(matches!(errors[1].reason, Some(Reason::ReturnShape)));
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
                    reason: None,
                },
                Key {
                    label: "early",
                    holds: Holds::At(Ty::bool),
                    reason: None,
                },
                Key {
                    label: "required",
                    holds: Holds::Required(Ty::string),
                    reason: None,
                },
            ],
            factory: false,
        };
        let mut generator = generator();
        let opts = FieldsNode::new(
            vec![
                ("later".into(), Spanned::new(at(5), Val::Int(1))),
                ("early".into(), Spanned::new(at(2), Val::Int(2))),
            ]
            .into(),
        );
        let nodes = generator.check_options(&opts, &TABLE);
        assert_eq!(nodes.shape()[0].0.as_ref(), "later");
        assert_eq!(nodes.shape()[0].1.span, Some(at(5)));
        assert_eq!(nodes.shape()[1].0.as_ref(), "early");
        assert_eq!(nodes.shape()[1].1.span, Some(at(2)));
        let errors = generator.solver.refused().1;
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors.iter().any(|error| matches!(
            &error.kind, TypeErrorKind::RowMissingField { label } if label == "required"
        )));
        let positions: Vec<_> = errors.iter().filter_map(|error| error.pos).collect();
        assert_eq!(positions, [at(2), at(5)]);
    }

    #[test]
    fn grant_options_keep_unknown_refused_and_vocabulary_refusals_distinct() {
        let mut generator = generator();
        let caps = FieldsNode::new(
            vec![
                ("missing".into(), Spanned::new(at(1), Val::Unit)),
                ("detach".into(), Spanned::new(at(2), Val::Bool(true))),
                ("net".into(), Spanned::new(at(3), Val::Int(1))),
            ]
            .into(),
        );
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
    fn grant_nested_tables_are_checked_at_close_without_constraining_their_values() {
        let mut generator = generator();
        let unknown = generator.solver.fresh_ty();
        generator
            .env
            .bind("caps".into(), Scheme::mono(unknown.clone()));
        let caps = FieldsNode::new(
            vec![(
                "exec".into(),
                Spanned::new(at(2), Val::Variable("caps".into())),
            )]
            .into(),
        );
        generator.grant(&caps, &block(command(3, "emit")));
        assert!(
            generator
                .solver
                .equate(&unknown, &Ty::string(), Reason::Argument)
        );
        let (store, errors) = generator.solver.refused();
        let [error] = &errors[..] else {
            panic!("one nested-table refusal: {errors:?}")
        };
        assert!(matches!(
            error.kind,
            TypeErrorKind::NotATable { key: "exec", .. }
        ));
        assert_eq!(error.pos, Some(at(2)));
        assert_eq!(store.resolve_ty(&unknown), Ty::string());
    }

    #[test]
    fn within_installs_named_arms_only_for_its_body_and_keeps_arm_spans() {
        let mut generator = generator();
        let original = Arc::new(Scheme::mono(Ty::thunk(CompTy::arrows(
            [Ty::argv()],
            CompTy::pure(Ty::int()),
        ))));
        generator
            .env
            .bind_handler("tool".into(), Arc::clone(&original), HandlerOrigin::Alias);
        let arms = [HandlerArmV {
            name: "tool".into(),
            value: Spanned::new(
                at(1),
                block(node(
                    2,
                    CompKind::Lam {
                        param: "args".into(),
                        body: Arc::new(node(3, CompKind::Return(Val::Int(1)))),
                    },
                )),
            ),
        }];
        let (cty, item) = generator.within(
            &FieldsNode::new(Vec::new().into()),
            Some(&arms),
            &block(command(4, "tool")),
        );
        let CompKind::Within {
            handlers: Some(arms),
            ..
        } = item
        else {
            panic!("named arms preserved")
        };
        assert_eq!(arms[0].value.span, Some(at(1)));
        let outer = generator.env.lookup_handler("tool").unwrap();
        assert!(Arc::ptr_eq(&outer.scheme, &original));
        assert!(outer.origin == HandlerOrigin::Alias);
        assert!(generator.env.lookup_binding("args").is_none());
        let solution = solved(generator.solver);
        assert_eq!(solution.resolve_comp(&cty), CompTy::pure(Ty::int()));
    }

    #[test]
    fn retained_protection_is_kept_beside_the_bind_and_call_holes_inside_scopes() {
        let closed = solved(Solver::new());
        let site = Arc::new(closed.site(&Ty::int(), UnitId::fresh()));
        let app = node(
            3,
            CompKind::App {
                head: Arc::new(node(
                    4,
                    CompKind::Lam {
                        param: "x".into(),
                        body: Arc::new(node(5, CompKind::Return(Val::Variable("x".into())))),
                    },
                )),
                args: vec![Spanned::new(at(6), Val::Int(1))].into(),
                hole: (),
            },
        );
        let protected = node(
            2,
            CompKind::Protect {
                body: Arc::new(app),
                site: Arc::clone(&site),
            },
        );
        let body = block(node(
            1,
            CompKind::bind(
                Pattern::Name("result".into()),
                protected,
                node(7, CompKind::Return(Val::Variable("result".into()))),
            ),
        ));
        let mut generator = generator();
        let (cty, item) = generator.gather(&body);
        let CompKind::Gather {
            body: Val::Thunk(body),
        } = &item
        else {
            panic!("gather body preserved")
        };
        let CompKind::Bind {
            comp: retained,
            hole: bound,
            ..
        } = &body.shape().item
        else {
            panic!("bind preserved")
        };
        let CompKind::Protect {
            body: app,
            site: kept,
        } = &retained.item
        else {
            panic!("retained protection")
        };
        assert!(Arc::ptr_eq(kept, &site));
        assert_eq!(retained.span, Some(at(2)));
        let CompKind::App { head, hole, .. } = &app.item else {
            panic!("application preserved")
        };
        assert!(matches!(head.item, CompKind::Lam { .. }));
        let (bound, call) = (*bound, *hole);
        let solution = solved(generator.solver);
        assert_solved_comp(&solution.scratch(), &cty, &CompTy::pure(Ty::int()));
        assert_eq!(solution.mode(bound), Mode::Run);
        assert_eq!(solution.splits(call), Vec::<usize>::new());
        let checked = Readback::new(&solution, UnitId::fresh(), 0).comp(&Spanned::new(at(0), item));
        let CompKind::Gather {
            body: Val::Thunk(body),
        } = checked.item
        else {
            panic!("checked gather")
        };
        let CompKind::Bind { comp: rhs, .. } = &body.shape().item else {
            panic!("checked bind")
        };
        let CompKind::Protect { site: kept, .. } = &rhs.item else {
            panic!("actual protection survives readback")
        };
        assert!(Arc::ptr_eq(kept, &site));
    }

    #[test]
    fn protected_comp_checks_only_the_result_not_the_producer_grade() {
        for (want, accepted) in [(Ty::unit(), true), (Ty::string(), false)] {
            let closed = solved(Solver::new());
            let site = Arc::new(closed.site(&want, UnitId::fresh()));
            let mut generator = generator();
            let (cty, _) = generator.protected_comp(&command(1, "emit"), &site);
            if accepted {
                assert_eq!(
                    solved(generator.solver).resolve_comp(&cty),
                    CompTy::command()
                );
            } else {
                let (store, errors) = generator.solver.refused();
                assert_eq!(errors.len(), 1, "{errors:?}");
                assert!(matches!(errors[0].reason, Some(Reason::ProtectContract)));
                assert_eq!(errors[0].pos, Some(at(1)));
                assert_eq!(store.resolve_comp_ty_graded(&cty), CompTy::command());
            }
        }
    }
}
