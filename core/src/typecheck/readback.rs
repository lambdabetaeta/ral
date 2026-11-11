//! Fill elaboration holes using a closed solution; shared CBPV node builders.

use super::env::{Inferring, Membrane};
use super::generate::LiveSchemes;
use super::solve::{BindHole, Mode, Solution};
use crate::ir::{
    Argv, CaseArm, CommandName, CommandWord, Comp, CompKind, Exec, FieldsNode, GroupNode,
    HandlerArmV, Name, Pattern, Phase, Phrase, Prim, Redirects, Toplevel, Val, ValListElem,
    synthetic,
};
use crate::source::{Span, Spanned};
use crate::ty::{Site, Ty, UnitId};
use std::collections::HashMap;
use std::sync::Arc;

pub(super) struct Readback<'a> {
    solution: &'a Solution,
    unit: UnitId,
    groups: HashMap<*const GroupNode<Inferring>, Arc<GroupNode>>,
    synth_counter: usize,
}

impl<'a> Readback<'a> {
    pub(super) fn new(solution: &'a Solution, unit: UnitId, synth_counter: usize) -> Self {
        Self {
            solution,
            unit,
            groups: HashMap::new(),
            synth_counter,
        }
    }

    pub(super) fn toplevel(
        &mut self,
        phrases: &[Spanned<Phrase<LiveSchemes, Inferring>>],
        residual_uses: &[(String, Ty)],
    ) -> Toplevel {
        let phrases = phrases
            .iter()
            .map(|phrase| {
                let item = match &phrase.item {
                    Phrase::Define {
                        pattern,
                        comp,
                        hole,
                        schemes,
                    } => {
                        let schemes = schemes
                            .iter()
                            .map(|(name, scheme)| {
                                (name.clone(), self.solution.settle(Arc::clone(scheme)))
                            })
                            .collect();
                        Phrase::Define {
                            pattern: Arc::clone(pattern),
                            comp: Arc::new(self.bound(comp, *hole)),
                            hole: (),
                            schemes,
                        }
                    }
                    Phrase::Run(comp) => Phrase::Run(Arc::new(self.comp(comp))),
                };
                Spanned::with_span(phrase.span, item)
            })
            .collect();
        let admits = residual_uses
            .iter()
            .map(|(name, ty)| (name.clone(), self.site(ty)))
            .collect();
        Toplevel { phrases, admits }
    }

    fn bound(&mut self, rhs: &Comp<Inferring>, hole: BindHole) -> Comp {
        let body = self.comp(rhs);
        match self.solution.mode(hole) {
            Mode::Run => body,
            Mode::Capture => {
                Spanned::with_span(rhs.span, captured_string(body, &mut self.synth_counter))
            }
        }
    }

    fn site(&self, ty: &Ty) -> Arc<Site> {
        Arc::new(self.solution.site(ty, self.unit))
    }

    fn rec_group(&mut self, group: &Arc<GroupNode<Inferring>>) -> Arc<GroupNode> {
        let key = Arc::as_ptr(group);
        if let Some(rebuilt) = self.groups.get(&key) {
            return Arc::clone(rebuilt);
        }
        let rebuilt = GroupNode::new(
            group
                .shape()
                .iter()
                .map(|(name, m)| (name.clone(), Arc::new(self.comp(m))))
                .collect::<Vec<_>>()
                .into(),
        );
        self.groups.insert(key, Arc::clone(&rebuilt));
        rebuilt
    }

    pub(super) fn comp(&mut self, comp: &Comp<Inferring>) -> Comp {
        let item = match &comp.item {
            CompKind::Bind {
                comp: rhs,
                hole,
                pattern,
                rest,
            } => CompKind::Bind {
                comp: Arc::new(self.bound(rhs, *hole)),
                hole: (),
                pattern: Arc::clone(pattern),
                rest: Arc::new(self.comp(rest)),
            },
            CompKind::Seq { first, rest } => CompKind::Seq {
                first: Arc::new(self.comp(first)),
                rest: Arc::new(self.comp(rest)),
            },
            CompKind::Pipeline { stages } => CompKind::Pipeline {
                stages: stages
                    .iter()
                    .map(|stage| Arc::new(self.comp(stage)))
                    .collect(),
            },
            CompKind::Lam { param, body } => CompKind::Lam {
                param: param.clone(),
                body: Arc::new(self.comp(body)),
            },
            CompKind::App { head, args, hole } => {
                let head = self.comp(head);
                let args = args.iter().map(|arg| self.spanned_val(arg)).collect();
                split(
                    comp.span,
                    self.solution.splits(*hole),
                    args,
                    &mut self.synth_counter,
                    |args| applied(comp.span, head, args),
                )
            }
            CompKind::BindingCall {
                head,
                args,
                redirects,
                hole,
            } => {
                let args = args.iter().map(|arg| self.spanned_val(arg)).collect();
                let redirects = redirects.map(|v| self.val(v));
                exec_call(
                    comp.span,
                    &CommandWord::Name(CommandName::Bare(head.clone())),
                    args,
                    self.solution.splits(*hole),
                    redirects,
                    &mut self.synth_counter,
                )
            }
            CompKind::Protect { body, site } => CompKind::Protect {
                body: Arc::new(self.comp(body)),
                site: Arc::clone(site),
            },
            CompKind::Force(value) => CompKind::Force(self.val(value)),
            CompKind::Return(value) => CompKind::Return(self.val(value)),
            CompKind::Exec(e) => exec_node(
                &e.head,
                self.argv(&e.argv),
                e.redirects.map(|v| self.val(v)),
                e.site.as_ref().map(|ty| self.site(ty)),
            ),
            CompKind::Prim(prim) => CompKind::Prim(prim.map_operands(|value, _| self.val(value))),
            CompKind::Rec { group, index } => CompKind::Rec {
                group: self.rec_group(group),
                index: *index,
            },
            CompKind::Tilde(path) => CompKind::Tilde(path.clone()),
            CompKind::If { cond, then, else_ } => CompKind::If {
                cond: self.spanned_val(cond),
                then: self.spanned_val(then),
                else_: self.spanned_val(else_),
            },
            CompKind::Case { scrutinee, arms } => CompKind::Case {
                scrutinee: self.spanned_val(scrutinee),
                arms: arms
                    .iter()
                    .map(|arm| CaseArm {
                        tag: arm.tag.clone(),
                        body: self.spanned_val(&arm.body),
                    })
                    .collect(),
            },
            CompKind::Try { body, handler } => CompKind::Try {
                body: self.val(body),
                handler: self.val(handler),
            },
            CompKind::Guard {
                acquire,
                body,
                release,
            } => CompKind::Guard {
                acquire: self.val(acquire),
                body: self.val(body),
                release: self.val(release),
            },
            CompKind::Within {
                opts,
                handlers,
                body,
            } => CompKind::Within {
                opts: self.fields(opts),
                handlers: handlers.as_ref().map(|arms| {
                    arms.iter()
                        .map(|arm| HandlerArmV {
                            name: arm.name.clone(),
                            value: self.spanned_val(&arm.value),
                        })
                        .collect()
                }),
                body: self.val(body),
            },
            CompKind::Grant { caps, body } => CompKind::Grant {
                caps: self.fields(caps),
                body: self.val(body),
            },
            CompKind::Audit { body } => CompKind::Audit {
                body: self.val(body),
            },
            CompKind::Spawn { title, body } => CompKind::Spawn {
                title: title.as_ref().map(|t| self.spanned_val(t)),
                body: self.val(body),
            },
            CompKind::Gather { body } => CompKind::Gather {
                body: self.val(body),
            },
            CompKind::Redirect { body, redirects } => CompKind::Redirect {
                body: Arc::new(self.comp(body)),
                redirects: redirects.map(|v| self.val(v)),
            },
            CompKind::Capture(body) => CompKind::Capture(Arc::new(self.comp(body))),
        };
        Spanned::with_span(comp.span, item)
    }

    pub(super) fn val(&mut self, val: &Val<Inferring>) -> Val {
        match val {
            Val::Protected { value, site } => {
                let value = self.val(value);
                let site = match site {
                    Membrane::Kept(site) => Arc::clone(site),
                    Membrane::Imported(ty) => {
                        let site = self.site(ty);
                        if !site.has_delayed() {
                            return value;
                        }
                        site
                    }
                };
                Val::Protected {
                    value: Box::new(value),
                    site,
                }
            }
            Val::Thunk(comp) => Val::thunk(Arc::new(self.comp(comp.shape()))),
            Val::List(elems) => Val::list(
                elems
                    .shape()
                    .iter()
                    .map(|v| self.spanned_val(v))
                    .collect::<Vec<_>>(),
            ),
            Val::Record(entries) => Val::Record(self.fields(entries)),
            Val::Map(entries) => Val::Map(self.fields(entries)),
            Val::Variant { label, payload } => Val::Variant {
                label: label.clone(),
                payload: Box::new(self.val(payload)),
            },
            Val::Unit => Val::Unit,
            Val::String(v) => Val::String(v.clone()),
            Val::Int(v) => Val::Int(*v),
            Val::Float(v) => Val::Float(*v),
            Val::Bool(v) => Val::Bool(*v),
            Val::Variable(v) => Val::Variable(v.clone()),
        }
    }

    fn spanned_val(&mut self, value: &Spanned<Val<Inferring>>) -> Spanned<Val> {
        Spanned::with_span(value.span, self.val(&value.item))
    }

    fn list_elem(&mut self, elem: &ValListElem<Inferring>) -> ValListElem {
        match elem {
            ValListElem::Single(v) => ValListElem::Single(self.spanned_val(v)),
            ValListElem::Spread(v) => ValListElem::Spread(self.spanned_val(v)),
        }
    }

    fn argv(&mut self, argv: &Argv<Inferring>) -> Argv {
        argv.iter().map(|e| self.list_elem(e)).collect()
    }

    fn fields(&mut self, fields: &FieldsNode<Inferring>) -> Arc<FieldsNode> {
        FieldsNode::new(
            fields
                .shape()
                .iter()
                .map(|(name, value)| (name.clone(), self.spanned_val(value)))
                .collect(),
        )
    }
}

fn split(
    span: Option<Span>,
    splits: &[usize],
    mut args: Vec<Spanned<Val>>,
    counter: &mut usize,
    first: impl FnOnce(Vec<Spanned<Val>>) -> Comp,
) -> CompKind {
    let mut later: Vec<_> = splits.iter().rev().map(|&k| args.split_off(k)).collect();
    later.reverse();
    let mut call = first(args);
    let mut binds = Vec::with_capacity(later.len());
    for segment in later {
        let thunk = fresh_name(counter, "split");
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

fn exec_call(
    span: Option<Span>,
    head: &CommandWord,
    args: Vec<Spanned<Val>>,
    splits: &[usize],
    redirects: Redirects<Val>,
    counter: &mut usize,
) -> CompKind {
    if splits.is_empty() {
        exec_node(
            head,
            args.into_iter().map(ValListElem::Single).collect(),
            redirects,
            None,
        )
    } else {
        let call = split(span, splits, args, counter, |args| {
            Spanned::with_span(
                span,
                exec_node(
                    head,
                    args.into_iter().map(ValListElem::Single).collect(),
                    Redirects::default(),
                    None,
                ),
            )
        });
        redirected(Spanned::with_span(span, call), redirects)
    }
}

pub(super) fn fresh_name(counter: &mut usize, purpose: &str) -> Name {
    *counter += 1;
    synthetic(purpose, *counter)
}

fn captured_string(body: Comp, counter: &mut usize) -> CompKind {
    let span = body.span;
    let name = fresh_name(counter, "decode");
    CompKind::bind(
        Pattern::Name(name.clone()),
        Spanned::with_span(span, CompKind::Capture(Arc::new(body))),
        Spanned::with_span(span, CompKind::Prim(Prim::Decode(Val::Variable(name)))),
    )
}

pub(super) fn eta<P: Phase>(counter: &mut usize, n: usize) -> (Vec<Name>, Vec<Spanned<Val<P>>>) {
    let params: Vec<Name> = (0..n).map(|_| fresh_name(counter, "eta")).collect();
    let args = params
        .iter()
        .map(|param| Spanned::synthetic(Val::Variable(param.clone())))
        .collect();
    (params, args)
}

fn applied(span: Option<Span>, head: Comp, args: Vec<Spanned<Val>>) -> Comp {
    if args.is_empty() {
        head
    } else {
        Spanned::with_span(
            span,
            CompKind::App {
                head: Arc::new(head),
                args: args.into(),
                hole: (),
            },
        )
    }
}

pub(super) fn exec_node<P: Phase>(
    head: &CommandWord,
    argv: Argv<P>,
    redirects: Redirects<Val<P>>,
    site: Option<P::Boundary>,
) -> CompKind<P> {
    CompKind::Exec(Exec {
        head: head.clone(),
        argv,
        redirects,
        site,
    })
}

pub(super) fn redirected<P: Phase>(body: Comp<P>, redirects: Redirects<Val<P>>) -> CompKind<P> {
    if redirects.is_empty() {
        body.item
    } else {
        CompKind::Redirect {
            body: Arc::new(body),
            redirects,
        }
    }
}

pub(super) fn lambda<P: Phase>(span: Option<Span>, params: Vec<Name>, body: CompKind<P>) -> Val<P> {
    Val::thunk(Arc::new(lambda_comp(span, params, body)))
}

pub(super) fn lambda_comp<P: Phase>(
    span: Option<Span>,
    params: Vec<Name>,
    body: CompKind<P>,
) -> Comp<P> {
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

pub(super) fn boundary_block<P: Phase>(
    name: &str,
    arity: usize,
    site: P::Boundary,
    span: Option<Span>,
    counter: &mut usize,
) -> Val<P> {
    let (params, args) = eta(counter, arity);
    let call = exec_node(
        &CommandWord::Name(CommandName::Bare(name.into())),
        args.into_iter().map(ValListElem::Single).collect(),
        Redirects::default(),
        Some(site),
    );
    lambda(span, params, call)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::StdinSource;
    use crate::source::FileId;
    use crate::ty::{Binders, CompTy, Grade};
    use crate::typecheck::solve::Solver;

    fn span() -> Span {
        Span::new(FileId::DUMMY, 3, 17)
    }

    fn node<P: Phase>(item: CompKind<P>) -> Comp<P> {
        Spanned::with_span(Some(span()), item)
    }

    fn success(solver: Solver) -> Solution {
        solver
            .settle()
            .close()
            .unwrap_or_else(|errors| panic!("{errors:?}"))
    }

    fn command() -> CompTy {
        CompTy::ret(Grade::Output, Ty::unit())
    }

    fn thunk(body: CompTy) -> Ty {
        Ty::thunk(body)
    }

    fn force_f<P: Phase>() -> Comp<P> {
        node(CompKind::Force(Val::Variable("f".into())))
    }

    #[test]
    fn bound_holes_run_or_capture_with_supplied_names_and_spans() {
        let cases = [
            (CompTy::pure(Ty::int()), Mode::Run),
            (
                CompTy::arrows([Ty::int()], CompTy::pure(Ty::int())),
                Mode::Run,
            ),
            (command(), Mode::Capture),
        ];
        for (ty, mode) in cases {
            let mut solver = Solver::new();
            let (_, hole) = solver.binding(&ty, None);
            let solution = success(solver);
            assert_eq!(solution.mode(hole), mode);
            let mut walk = Readback::new(&solution, UnitId::fresh(), 40);
            let result = walk.bound(&force_f(), hole);
            assert_eq!(result.span, Some(span()));
            let expected = match mode {
                Mode::Run => force_f(),
                Mode::Capture => {
                    let name = synthetic("decode", 41);
                    node(CompKind::bind(
                        Pattern::Name(name.clone()),
                        node(CompKind::Capture(Arc::new(force_f()))),
                        node(CompKind::Prim(Prim::Decode(Val::Variable(name)))),
                    ))
                }
            };
            assert_eq!(result, expected);
        }
    }

    #[test]
    fn call_hole_preserves_repeated_zero_based_splits_and_argument_spans() {
        let ty = CompTy::pure(thunk(CompTy::pure(thunk(CompTy::arrows(
            [Ty::int()],
            CompTy::pure(Ty::int()),
        )))));
        let mut solver = Solver::new();
        let call = solver.call();
        solver.argument(call, &ty, None, Ty::int(), false);
        let solution = success(solver);
        assert_eq!(solution.splits(call), [0, 0]);
        let pending = node(CompKind::App {
            head: Arc::new(force_f()),
            args: vec![Spanned::with_span(Some(span()), Val::Int(7))].into(),
            hole: call,
        });
        let mut walk = Readback::new(&solution, UnitId::fresh(), 10);
        let first = synthetic("split", 11);
        let second = synthetic("split", 12);
        assert_eq!(
            walk.comp(&pending),
            node(CompKind::bind(
                Pattern::Name(first.clone()),
                force_f(),
                node(CompKind::bind(
                    Pattern::Name(second.clone()),
                    node(CompKind::Force(Val::Variable(first))),
                    applied(
                        Some(span()),
                        node(CompKind::Force(Val::Variable(second))),
                        vec![Spanned::with_span(Some(span()), Val::Int(7))]
                    ),
                )),
            ))
        );
    }

    #[test]
    fn binding_call_fuses_unsplit_redirects_but_wraps_the_entire_split() {
        for split in [false, true] {
            let arrow = CompTy::arrows([Ty::int()], command());
            let ty = if split {
                CompTy::pure(thunk(arrow))
            } else {
                arrow
            };
            let mut solver = Solver::new();
            let call = solver.call();
            solver.argument(call, &ty, None, Ty::int(), false);
            let solution = success(solver);
            let mut walk = Readback::new(&solution, UnitId::fresh(), 0);
            let redirects: Redirects<Val> = Redirects {
                stdin: Some(StdinSource::Here(Val::String("input".into()))),
                ..Redirects::default()
            };
            let pending = node(CompKind::BindingCall {
                head: "f".into(),
                args: vec![Spanned::with_span(Some(span()), Val::Int(1))].into(),
                redirects: redirects.map(|_| Val::String("input".into())),
                hole: call,
            });
            let result = walk.comp(&pending);
            if split {
                let CompKind::Redirect {
                    body,
                    redirects: found,
                } = &result.item
                else {
                    panic!("redirect must enclose the split")
                };
                assert_eq!(found, &redirects);
                let CompKind::Bind { comp, .. } = &body.item else {
                    panic!("expected split")
                };
                let CompKind::Exec(exec) = &comp.item else {
                    panic!("expected first call")
                };
                assert!(exec.redirects.is_empty());
                assert!(exec.argv.is_empty());
                assert!(exec.site.is_none());
            } else {
                let CompKind::Exec(exec) = result.item else {
                    panic!("expected unsplit call")
                };
                assert_eq!(exec.head, CommandWord::Name(CommandName::Bare("f".into())));
                assert_eq!(exec.redirects, redirects);
                assert_eq!(exec.argv.arity(), Some(1));
                assert!(exec.site.is_none());
            }
        }
    }

    #[test]
    fn elaborated_capture_and_repeated_splits_reinfer_unchanged() {
        let arrow = CompTy::arrows([Ty::int()], CompTy::pure(Ty::int()));
        let ty = CompTy::pure(thunk(CompTy::pure(thunk(arrow))));
        let mut solver = Solver::new();
        let (_, bound) = solver.binding(&command(), None);
        let call = solver.call();
        solver.argument(call, &ty, None, Ty::int(), false);
        let solution = success(solver);
        let capture = node(CompKind::Bind {
            comp: Arc::new(node(exec_node(
                &CommandWord::External(CommandName::Bare("echo".into())),
                vec![ValListElem::Single(Spanned::synthetic(Val::String(
                    "hi".into(),
                )))]
                .into(),
                Redirects::default(),
                None,
            ))),
            hole: bound,
            pattern: Arc::new(Pattern::Name("output".into())),
            rest: Arc::new(node(CompKind::Return(Val::Variable("output".into())))),
        });
        let function = node(CompKind::Lam {
            param: "n".into(),
            body: Arc::new(node(CompKind::Return(Val::Variable("n".into())))),
        });
        let head = node(CompKind::Return(Val::thunk(Arc::new(node(
            CompKind::Return(Val::thunk(Arc::new(function))),
        )))));
        let application = node(CompKind::App {
            head: Arc::new(head),
            args: vec![Spanned::with_span(Some(span()), Val::Int(7))].into(),
            hole: call,
        });
        for pending in [capture, application] {
            let checked = Readback::new(&solution, UnitId::fresh(), 40).toplevel(
                &[Spanned::with_span(
                    Some(span()),
                    Phrase::Run(Arc::new(pending)),
                )],
                &[],
            );
            let Phrase::Run(body) = &checked.phrases[0].item else {
                panic!("expected Run");
            };
            let rechecked = crate::typecheck::typecheck(
                &[Spanned::with_span(
                    Some(span()),
                    Phrase::Run(Arc::clone(body)),
                )],
                crate::test_helper::core_schemes(),
                None,
            )
            .expect("elaboration must re-infer without further changes");
            assert_eq!(rechecked, checked);
        }
    }

    #[test]
    fn sites_keep_identity_and_imports_keep_only_delayed_protection() {
        let solution = success(Solver::new());
        let unit = UnitId::fresh();
        let kept = Arc::new(solution.site(&Ty::int(), UnitId::fresh()));
        let mut walk = Readback::new(&solution, unit, 0);
        for membrane in [
            Membrane::Kept(Arc::clone(&kept)),
            Membrane::Imported(Ty::int()),
            Membrane::Imported(thunk(command())),
        ] {
            let imported = matches!(&membrane, Membrane::Imported(_));
            let value = walk.val(&Val::Protected {
                value: Box::new(Val::Variable("x".into())),
                site: membrane,
            });
            match value {
                Val::Variable(_) => assert!(imported),
                Val::Protected { site, .. } => {
                    if site.unit == kept.unit {
                        assert!(Arc::ptr_eq(&kept, &site));
                    } else {
                        assert_eq!(site.unit, unit);
                        assert!(!imported || site.has_delayed());
                    }
                }
                _ => panic!("unexpected protected value"),
            }
        }
        let boundary = walk.comp(&node(exec_node(
            &CommandWord::Name(CommandName::Bare("decode".into())),
            Argv::default(),
            Redirects::default(),
            Some(Ty::int()),
        )));
        let CompKind::Exec(Exec {
            site: Some(site), ..
        }) = boundary.item
        else {
            panic!("a boundary call keeps its site")
        };
        assert_eq!(site.unit, unit);
    }

    #[test]
    fn toplevel_settles_schemes_and_readmissions_in_the_same_unit() {
        let mut solver = Solver::new();
        let ty = solver.fresh_ty();
        let scheme = Arc::new(solver.generalise(&ty));
        solver.equate(&ty, &Ty::int(), crate::typecheck::error::Reason::Argument);
        let (_, hole) = solver.binding(&CompTy::pure(ty.clone()), None);
        let solution = success(solver);
        let unit = UnitId::fresh();
        let mut walk = Readback::new(&solution, unit, 0);
        let pending = Spanned::with_span(
            Some(span()),
            Phrase::Define {
                pattern: Arc::new(Pattern::Name("x".into())),
                comp: Arc::new(node(CompKind::Return(Val::Int(1)))),
                hole,
                schemes: vec![("x".into(), scheme)],
            },
        );
        let result = walk.toplevel(&[pending], &[("imported".into(), ty)]);
        assert_eq!(result.phrases[0].span, Some(span()));
        let Phrase::Define { schemes, comp, .. } = &result.phrases[0].item else {
            panic!("expected definition")
        };
        assert_eq!(schemes, &vec![("x".into(), Binders::close(|_| Ty::int()))]);
        assert_eq!(comp.as_ref(), &node(CompKind::Return(Val::Int(1))));
        assert_eq!(result.admits[0].0, "imported");
        assert_eq!(result.admits[0].1.unit, unit);
        assert_eq!(*result.admits[0].1, solution.site(&Ty::int(), unit));
    }

    #[test]
    fn recursive_group_projections_share_the_rebuilt_group() {
        let solution = success(Solver::new());
        let mut walk = Readback::new(&solution, UnitId::fresh(), 0);
        let group: Arc<GroupNode<Inferring>> =
            GroupNode::new(vec![("f".into(), Arc::new(node(CompKind::Return(Val::Unit))))].into());
        let projection = node(CompKind::Rec { group, index: 0 });
        let first = walk.comp(&projection);
        let second = walk.comp(&projection);
        let (CompKind::Rec { group: a, .. }, CompKind::Rec { group: b, .. }) =
            (first.item, second.item)
        else {
            panic!("expected projections")
        };
        assert!(Arc::ptr_eq(&a, &b));
    }
}
