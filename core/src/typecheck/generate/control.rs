//! Joins, cases, pipelines, redirects and capture.

use super::Generator;
use crate::fact::ErrorRecord;
use crate::ir::{
    CaseArm, CommandName, CommandWord, Comp, CompKind, Redirects, StdinSource, Val, is_synthetic,
};
use crate::source::{Span, Spanned, WithSpan};
use crate::ty::{CompTy, Label, Producer, Row, Ty, Typed as _};
use crate::typecheck::builtins::BuiltinDiagnostic;
use crate::typecheck::env::Inferring;
use crate::typecheck::error::{Reason, StdinFeed, TypeErrorKind};
use std::sync::Arc;

pub(super) struct JoinArm<'a> {
    value: &'a Val,
    span: Option<Span>,
    params: Vec<Ty>,
    why: Reason,
}

impl<'a> JoinArm<'a> {
    pub(super) fn new(value: &'a Spanned<Val>, params: Vec<Ty>, why: Reason) -> Self {
        Self::in_hand(&value.item, value.span, params, why)
    }

    pub(super) fn in_hand(
        value: &'a Val,
        span: Option<Span>,
        params: Vec<Ty>,
        why: Reason,
    ) -> Self {
        Self {
            value,
            span,
            params,
            why,
        }
    }

    fn shaped(&self, tail: CompTy) -> CompTy {
        CompTy::arrows(self.params.iter().cloned(), tail)
    }

    pub(super) fn result_span(&self) -> Option<Span> {
        match self.value {
            Val::Thunk(comp) => Generator::result_span(comp.shape()),
            Val::Protected { value, .. } => match value.as_ref() {
                Val::Thunk(comp) => Generator::result_span(comp.shape()),
                _ => self.span,
            },
            _ => self.span,
        }
    }
}

impl Generator {
    pub(super) fn join_arms(
        &mut self,
        arms: &[JoinArm<'_>],
        why: &Reason,
    ) -> (CompTy, Vec<Val<Inferring>>) {
        let (tails, nodes): (Vec<_>, Vec<_>) = arms
            .iter()
            .map(|arm| {
                let tail = self.solver.fresh_comp();
                let node = self.check_arm(arm, &arm.shaped(tail.clone()));
                (tail, node)
            })
            .unzip();
        let target = self.solver.fresh_comp();
        for (arm, tail) in arms.iter().zip(&tails) {
            self.with_span(arm.result_span(), |this| {
                this.solver.equate_comp(&target, tail, why.clone());
            });
        }
        (target, nodes)
    }

    fn check_arm(&mut self, arm: &JoinArm<'_>, expected: &CompTy) -> Val<Inferring> {
        self.with_span(arm.span, |this| match arm.value {
            Val::Thunk(comp) => {
                let node = this.scope(|this| this.check(comp.shape(), expected, arm.why.clone()));
                Val::thunk(Arc::new(node))
            }
            other => {
                let (ty, node) = this.val(other, arm.span);
                this.solver
                    .equate(&ty, &Ty::thunk(expected.clone()), arm.why.clone());
                node
            }
        })
    }

    pub(in crate::typecheck) fn if_(
        &mut self,
        cond: &Spanned<Val>,
        then: &Spanned<Val>,
        else_: &Spanned<Val>,
    ) -> (CompTy, CompKind<Inferring>) {
        let (cond_ty, cond_node) = self.val(&cond.item, cond.span);
        self.with_span(cond.span, |this| {
            this.solver.equate(&cond_ty, &Ty::bool(), Reason::IfCond);
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

    pub(in crate::typecheck) fn case(
        &mut self,
        scrutinee: &Spanned<Val>,
        arms: &[CaseArm],
    ) -> (CompTy, CompKind<Inferring>) {
        let (scrut_ty, scrut_node) = self.with_span(scrutinee.span, |this| {
            this.val(&scrutinee.item, scrutinee.span)
        });
        let row = self.solver.fresh_row();
        self.with_span(scrutinee.span, |this| {
            this.solver
                .equate(&scrut_ty, &Ty::variant(row.clone()), Reason::CaseScrutinee);
        });
        let payloads: Vec<_> = arms.iter().map(|_| self.solver.fresh_ty()).collect();
        let labels: Vec<_> = arms
            .iter()
            .map(|arm| Label::Case(arm.tag.item.clone()))
            .collect();
        let closed = labels
            .iter()
            .zip(&payloads)
            .rev()
            .fold(Row::empty(), |rest, (label, payload)| {
                Row::extend(label.clone(), payload.clone(), rest)
            });
        // Coverage also pushes each payload into its arm before generation.
        self.solver.equate_row(
            &row,
            &closed,
            Reason::CaseCoverage {
                scrutinee: row.clone(),
                arms: labels,
            },
        );
        let join_arms: Vec<_> = arms
            .iter()
            .zip(payloads)
            .map(|(arm, payload)| {
                let why = match arm.body.item {
                    Val::Thunk(_) => Reason::CaseArms,
                    _ => Reason::CaseArmHandler,
                };
                JoinArm::new(&arm.body, vec![payload], why)
            })
            .collect();
        let (joined, nodes) = self.join_arms(&join_arms, &Reason::CaseArms);
        (
            joined,
            CompKind::Case {
                scrutinee: Spanned::with_span(scrutinee.span, scrut_node),
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

    pub(in crate::typecheck) fn try_(
        &mut self,
        body: &Val,
        handler: &Val,
    ) -> (CompTy, CompKind<Inferring>) {
        let arms = [
            JoinArm::in_hand(body, None, vec![], Reason::ScopeBody),
            JoinArm::in_hand(handler, None, vec![ErrorRecord::ty()], Reason::TryHandler),
        ];
        let (cty, nodes) = self.join_arms(&arms, &Reason::TryArms);
        let mut nodes = nodes.into_iter();
        (
            cty,
            CompKind::Try {
                body: nodes.next().expect("try body"),
                handler: nodes.next().expect("try handler"),
            },
        )
    }

    pub(in crate::typecheck) fn pipeline(
        &mut self,
        stages: &[Arc<Comp>],
    ) -> (CompTy, CompKind<Inferring>) {
        debug_assert!(stages.len() >= 2, "Pipeline carries ≥2 stages");
        let mut last = None;
        let mut nodes = Vec::with_capacity(stages.len());
        for (position, stage) in stages.iter().enumerate() {
            if position > 0
                && let Some(feed) = stage_root_stdin_feed(stage)
            {
                self.with_span(stage.span, |this| {
                    this.solver.diagnose(TypeErrorKind::DeadPipeEdge { feed });
                });
            }
            let (cty, node) = self.comp(stage);
            let producer = Producer {
                grade: self.solver.fresh_grade(),
                ty: self.solver.fresh_ty(),
            };
            let tail = Self::discard_tail(stage);
            let why = Reason::PipelineStageShape {
                builtin: self.discarded_builtin_arity(tail),
            };
            let ready = self.with_span(tail.span, |this| {
                this.solver.equate_comp(&cty, &producer.clone().into(), why)
            });
            if ready && let Some(next) = stages.get(position + 1) {
                self.with_span(stage.span, |this| this.stage_writes(stage, next, &producer));
            }
            last = Some(producer.into());
            nodes.push(Arc::new(node));
        }
        (
            last.expect("≥2 stages by invariant above"),
            CompKind::Pipeline { stages: nodes },
        )
    }

    fn stage_writes(&mut self, stage: &Comp, next: &Comp, producer: &Producer) {
        if let Some(name) = self.stage_decoder(stage) {
            return self.solver.diagnose(TypeErrorKind::DecoderMidPipeline {
                name,
                next: Self::stage_head(next),
            });
        }
        let unit = Producer {
            grade: producer.grade,
            ty: Ty::unit(),
        };
        self.solver.equate_comp(
            &producer.clone().into(),
            &unit.into(),
            Reason::PipelineStageWrites {
                stage: Self::stage_head(stage),
                next: Self::stage_head(next),
            },
        );
    }

    fn stage_decoder(&self, stage: &Comp) -> Option<String> {
        let CompKind::Exec(exec) = &Self::discard_tail(stage).item else {
            return None;
        };
        let CommandWord::Name(CommandName::Bare(name)) = &exec.head else {
            return None;
        };
        if self.env.lookup_binding(name).is_some() {
            return None;
        }
        self.env
            .builtins
            .value(name)
            .filter(|entry| entry.diagnostic == BuiltinDiagnostic::Decoder)
            .map(|_| name.to_string())
    }

    fn stage_head(stage: &Comp) -> Option<String> {
        fn written(value: &Val) -> Option<String> {
            match value {
                Val::Variable(name) if !is_synthetic(name) => Some(name.to_string()),
                _ => None,
            }
        }
        match &Self::discard_tail(stage).item {
            CompKind::Exec(exec) => Some(exec.head.name().written().into_owned()),
            CompKind::Force(value) => written(value),
            CompKind::App { head, .. } => match &head.item {
                CompKind::Force(value) => written(value),
                _ => None,
            },
            _ => None,
        }
    }

    pub(super) fn redirected(&mut self, cty: &CompTy, stdout: bool) -> CompTy {
        let producer = self.force_return_shape(cty, Reason::RedirectShape);
        if stdout {
            self.produces(producer.ty)
        } else {
            producer.into()
        }
    }

    pub(in crate::typecheck) fn redirect(
        &mut self,
        body: &Comp,
        redirects: &Redirects<Val>,
    ) -> (CompTy, CompKind<Inferring>) {
        let redirects = redirects.map(|operand| self.val(operand, None).1);
        let (cty, body) = self.comp(body);
        (
            self.redirected(&cty, redirects.stdout.is_some()),
            CompKind::Redirect {
                body: Arc::new(body),
                redirects,
            },
        )
    }

    pub(in crate::typecheck) fn capture(&mut self, body: &Comp) -> (CompTy, CompKind<Inferring>) {
        let (cty, body) = self.comp(body);
        self.solver
            .equate_comp(&cty, &CompTy::command(), Reason::Capture);
        (
            self.produces(Ty::bytes()),
            CompKind::Capture(Arc::new(body)),
        )
    }
}

fn stage_root_stdin_feed(stage: &Comp) -> Option<StdinFeed> {
    let redirects = match &Generator::discard_tail(stage).item {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Argv, Key, Pattern, Prim, WriteMode};
    use crate::source::FileId;
    use crate::ty::{CompTyNode, Grade, Scheme, TyNode};
    use crate::typecheck::builtins::{Convention, Decl, scheme};
    use crate::typecheck::env::TyEnv;
    use crate::typecheck::generate::tests::solved;
    use crate::typecheck::readback::{Readback, exec_node};
    use crate::typecheck::solve::{Mode, Solution, Solver};
    use std::borrow::Cow;

    fn generator() -> Generator {
        Generator::new(Solver::new(), TyEnv::new(), Vec::new())
    }

    fn at(start: u32) -> Span {
        Span::new(FileId::DUMMY, start, start + 1)
    }

    fn node(start: u32, item: CompKind) -> Comp {
        Spanned::new(at(start), item)
    }

    fn block(body: Comp) -> Spanned<Val> {
        Spanned::with_span(body.span, Val::thunk(Arc::new(body)))
    }

    fn returning(start: u32, value: Val) -> Comp {
        node(start, CompKind::Return(value))
    }

    fn command(start: u32) -> Comp {
        node(
            start,
            exec_node(
                &CommandWord::External(CommandName::Bare("emit".into())),
                Argv::default(),
                Redirects::default(),
                None,
            ),
        )
    }

    fn producer(solution: &Solution, cty: &CompTy) -> (Grade, Ty) {
        let resolved = solution.resolve_comp(cty);
        let CompTyNode::Return(grade, ty) = resolved.node() else {
            panic!("ready producer")
        };
        (*grade, solution.resolve_ty(ty))
    }

    fn case_arm(tag: &str, body: Comp) -> CaseArm {
        CaseArm {
            tag: Spanned::synthetic(tag.into()),
            body: block(node(
                2,
                CompKind::Lam {
                    param: "payload".into(),
                    body: Arc::new(body),
                },
            )),
        }
    }

    #[test]
    fn join_keeps_its_arms_in_source_order() {
        let mut generator = generator();
        let arms = [
            block(returning(1, Val::Int(1))),
            block(returning(2, Val::Int(2))),
        ];
        let joins = arms
            .iter()
            .map(|arm| JoinArm::new(arm, vec![], Reason::IfBranches))
            .collect::<Vec<_>>();
        let (cty, nodes) = generator.join_arms(&joins, &Reason::IfBranches);
        for (i, value) in nodes.iter().enumerate() {
            let Val::Thunk(body) = value else {
                panic!("literal arm remains a thunk")
            };
            assert_eq!(body.shape().span, Some(at(u32::try_from(i).unwrap() + 1)));
        }
        assert_eq!(producer(&solved(generator.solver), &cty).1, Ty::int());
    }

    #[test]
    fn if_join_leaves_capture_to_the_solver() {
        let mut generator = generator();
        let unknown = generator.solver.fresh_comp();
        generator
            .env
            .bind("f".into(), Scheme::mono(Ty::thunk(unknown)));
        let cond = Spanned::new(at(1), Val::Bool(true));
        let then = block(node(2, CompKind::Force(Val::Variable("f".into()))));
        let else_ = block(command(3));
        let (cty, item) = generator.if_(&cond, &then, &else_);
        let (bound, hole) = generator.solver.binding(&cty, None);
        assert!(matches!(item, CompKind::If { .. }));
        assert!(generator.env.lookup_binding("payload").is_none());
        let solution = solved(generator.solver);
        assert_eq!(producer(&solution, &cty), (Grade::Output, Ty::unit()));
        assert_eq!(solution.resolve_ty(&bound), Ty::string());
        assert_eq!(solution.mode(hole), Mode::Capture);
    }

    #[test]
    fn case_coverage_pushes_payload_knowledge_before_checking_arms() {
        let mut generator = generator();
        let scrutinee = Spanned::new(
            at(1),
            Val::Variant {
                label: "text".into(),
                payload: Box::new(Val::String("not a block".into())),
            },
        );
        generator.case(
            &scrutinee,
            &[case_arm(
                "text",
                node(3, CompKind::Force(Val::Variable("payload".into()))),
            )],
        );
        assert!(generator.env.lookup_binding("payload").is_none());
        let errors = generator.solver.refused().1;
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].kind.code(), "T0072");
        assert_eq!(errors[0].pos, Some(at(3)));
    }

    #[test]
    fn case_coverage_retains_closed_and_open_row_verdicts() {
        for (tags, open, covered, missing, extra) in [
            (vec!["a", "b"], false, vec!["a", "b"], vec![], vec![]),
            (vec!["a", "b"], false, vec!["a"], vec!["b"], vec![]),
            (vec!["a"], false, vec!["a", "b"], vec![], vec!["b"]),
            (vec!["a"], false, vec!["b", "c"], vec!["a"], vec!["b", "c"]),
            (vec!["a"], true, vec!["a", "b"], vec![], vec![]),
            (vec!["a", "b"], true, vec!["a", "c"], vec!["b"], vec![]),
        ] {
            let mut generator = generator();
            let rest = if open {
                generator.solver.fresh_row()
            } else {
                Row::empty()
            };
            let row = tags.into_iter().rev().fold(rest, |rest, tag| {
                Row::extend(Label::Case(tag.into()), Ty::unit(), rest)
            });
            generator
                .env
                .bind("v".into(), Scheme::mono(Ty::variant(row)));
            let arms = covered
                .into_iter()
                .map(|tag| case_arm(tag, returning(3, Val::Unit)))
                .collect::<Vec<_>>();
            generator.case(&Spanned::new(at(1), Val::Variable("v".into())), &arms);
            if missing.is_empty() && extra.is_empty() {
                solved(generator.solver);
            } else {
                let errors = generator.solver.refused().1;
                assert_eq!(errors.len(), 1, "{errors:?}");
                let TypeErrorKind::CaseNotExhaustive {
                    missing: got_missing,
                    extra: got_extra,
                } = &errors[0].kind
                else {
                    panic!("expected coverage diagnostic: {errors:?}")
                };
                let labels = |tags: Vec<&str>| {
                    tags.into_iter()
                        .map(|tag| Label::Case(tag.into()).to_string())
                        .collect::<Vec<_>>()
                };
                assert_eq!(got_missing, &labels(missing));
                assert_eq!(got_extra, &labels(extra));
                assert!(matches!(
                    errors[0].reason,
                    Some(Reason::CaseCoverage { .. })
                ));
            }
        }
    }

    #[test]
    fn case_non_variant_failure_is_translated_by_its_reason() {
        let mut generator = generator();
        generator.case(
            &Spanned::new(at(1), Val::Int(1)),
            &[case_arm("a", returning(3, Val::Unit))],
        );
        let errors = generator.solver.refused().1;
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].kind.code(), "T0032");
        assert_eq!(errors[0].pos, Some(at(1)));
        assert!(matches!(errors[0].reason, Some(Reason::CaseScrutinee)));
    }

    #[test]
    fn try_joins_results_and_pushes_the_error_record_into_its_handler() {
        let mut generator = generator();
        let body = block(returning(1, Val::String("ok".into()))).item;
        let handler = block(node(
            2,
            CompKind::Lam {
                param: "error".into(),
                body: Arc::new(node(
                    3,
                    CompKind::Prim(Prim::Index {
                        target: Val::Variable("error".into()),
                        key: Spanned::new(at(4), Key::Label("frame".into())),
                    }),
                )),
            },
        ))
        .item;
        let (cty, item) = generator.try_(&body, &handler);
        assert!(matches!(item, CompKind::Try { .. }));
        assert!(generator.env.lookup_binding("error").is_none());
        assert_eq!(producer(&solved(generator.solver), &cty).1, Ty::string());
    }

    #[test]
    fn pipeline_preserves_its_final_producer_and_refuses_an_earlier_value() {
        let mut generator = generator();
        let stages = [Arc::new(command(1)), Arc::new(returning(2, Val::Int(2)))];
        let (cty, item) = generator.pipeline(&stages);
        let CompKind::Pipeline { stages: nodes } = item else {
            panic!("pipeline preserved")
        };
        assert_eq!(
            nodes.iter().map(|node| node.span).collect::<Vec<_>>(),
            [Some(at(1)), Some(at(2))]
        );
        assert_eq!(producer(&solved(generator.solver), &cty).1, Ty::int());

        let mut generator = self::generator();
        generator.pipeline(&[Arc::new(returning(1, Val::Int(1))), Arc::new(command(2))]);
        let errors = generator.solver.refused().1;
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(matches!(
            errors[0].reason,
            Some(Reason::PipelineStageWrites { .. })
        ));
        assert_eq!(errors[0].pos, Some(at(1)));
    }

    #[test]
    fn pipeline_failed_shape_leaves_its_value_unknown_and_reports_builtin_context() {
        for final_stage in [false, true] {
            let mut generator = generator();
            generator.env.builtins.push(
                vec![Decl::new(
                    Cow::Borrowed("decode-at"),
                    Convention::Value,
                    scheme::from_json_at,
                    "",
                    true,
                )]
                .into(),
            );
            let partial = Arc::new(node(
                2,
                exec_node(
                    &CommandWord::Name(CommandName::Bare("decode-at".into())),
                    Argv::default(),
                    Redirects::default(),
                    None,
                ),
            ));
            let stages = if final_stage {
                [Arc::new(command(1)), partial]
            } else {
                [partial, Arc::new(command(3))]
            };
            let (cty, _) = generator.pipeline(&stages);
            // A refused non-final shape must not also demand that the stage writes.
            let (store, errors) = generator.solver.refused();
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert!(
                matches!(&errors[0].kind, TypeErrorKind::BuiltinArity { name, expected: 1, got: 0 } if name == "decode-at")
            );
            assert_eq!(errors[0].pos, Some(at(2)));
            let resolved = store.resolve_comp_ty_graded(&cty);
            let CompTyNode::Return(grade, ty) = resolved.node() else {
                panic!("ready producer")
            };
            let payload = store.resolve_ty(ty);
            if final_stage {
                assert!(matches!(payload.node(), TyNode::Var(_)), "{payload:?}");
            } else {
                assert_eq!((*grade, payload), (Grade::Output, Ty::unit()));
            }
        }
    }

    #[test]
    fn pipeline_refuses_decoders_by_declaration_but_respects_shadowing() {
        for shadow in [false, true] {
            let mut generator = generator();
            let mut decl = Decl::new(
                Cow::Borrowed("decode"),
                Convention::Value,
                scheme::from_json,
                "",
                true,
            );
            decl.diagnostic = BuiltinDiagnostic::Decoder;
            generator.env.builtins.push(vec![decl].into());
            if shadow {
                generator
                    .env
                    .bind("decode".into(), Scheme::mono(Ty::thunk(CompTy::command())));
            }
            let decode = node(
                1,
                exec_node(
                    &CommandWord::Name(CommandName::Bare("decode".into())),
                    Argv::default(),
                    Redirects::default(),
                    None,
                ),
            );
            generator.pipeline(&[Arc::new(decode), Arc::new(command(2))]);
            if shadow {
                solved(generator.solver);
            } else {
                let errors = generator.solver.refused().1;
                assert_eq!(errors.len(), 1, "{errors:?}");
                assert!(
                    matches!(&errors[0].kind, TypeErrorKind::DecoderMidPipeline { name, next: Some(next) } if name == "decode" && next == "emit")
                );
            }
        }
    }

    #[test]
    fn pipeline_checks_only_root_stdin_feeds_after_hoisted_binds() {
        for feed in [
            StdinSource::File(Val::String("in".into())),
            StdinSource::Here(Val::String("in".into())),
        ] {
            let mut generator = generator();
            let fed = node(
                3,
                CompKind::Redirect {
                    body: Arc::new(command(4)),
                    redirects: Redirects {
                        stdin: Some(feed),
                        ..Redirects::default()
                    },
                },
            );
            let hoisted = node(
                2,
                CompKind::bind(Pattern::Wildcard, returning(2, Val::Unit), fed),
            );
            generator.pipeline(&[Arc::new(command(1)), Arc::new(hoisted)]);
            let errors = generator.solver.refused().1;
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert!(matches!(errors[0].kind, TypeErrorKind::DeadPipeEdge { .. }));
            assert_eq!(errors[0].pos, Some(at(2)));
        }
        let nested = node(
            1,
            CompKind::Redirect {
                body: Arc::new(command(2)),
                redirects: Redirects {
                    stdin: Some(StdinSource::File(Val::String("in".into()))),
                    ..Redirects::default()
                },
            },
        );
        let stage = node(
            0,
            CompKind::Seq {
                first: Arc::new(nested),
                rest: Arc::new(command(3)),
            },
        );
        assert!(stage_root_stdin_feed(&stage).is_none());
    }

    #[test]
    fn redirects_preserve_values_and_only_stdout_frees_the_output_grade() {
        for stdout in [false, true] {
            let mut generator = generator();
            let redirects = if stdout {
                Redirects {
                    stdout: Some((WriteMode::Write, Val::String("out".into()))),
                    ..Redirects::default()
                }
            } else {
                Redirects {
                    stdin: Some(StdinSource::Here(Val::String("in".into()))),
                    ..Redirects::default()
                }
            };
            let (cty, item) = generator.redirect(&command(1), &redirects);
            assert!(matches!(item, CompKind::Redirect { .. }));
            let (grade, ty) = producer(&solved(generator.solver), &cty);
            assert_eq!(ty, Ty::unit());
            assert!(if stdout {
                matches!(grade, Grade::Var(_))
            } else {
                grade == Grade::Output
            });
        }
    }

    #[test]
    fn capture_checks_output_and_generated_bind_and_app_holes_survive_control_wrappers() {
        let mut generator = generator();
        let function = node(
            2,
            CompKind::Lam {
                param: "x".into(),
                body: Arc::new(command(3)),
            },
        );
        let app = node(
            1,
            CompKind::App {
                head: Arc::new(function),
                args: vec![Spanned::new(at(4), Val::Int(1))].into(),
                hole: (),
            },
        );
        let body = node(0, CompKind::bind(Pattern::Wildcard, app, command(5)));
        let (cty, item) = generator.capture(&body);
        let CompKind::Capture(body) = &item else {
            panic!("capture preserved")
        };
        let CompKind::Bind { comp, hole, .. } = &body.item else {
            panic!("bind preserved")
        };
        let bound = *hole;
        let CompKind::App { hole, .. } = &comp.item else {
            panic!("application preserved")
        };
        let call = *hole;
        let solution = solved(generator.solver);
        assert_eq!(producer(&solution, &cty).1, Ty::bytes());
        assert_eq!(solution.mode(bound), Mode::Capture);
        assert_eq!(solution.splits(call), Vec::<usize>::new());
        Readback::new(&solution, crate::ty::UnitId::fresh(), 0).comp(&Spanned::new(at(0), item));
    }
}
