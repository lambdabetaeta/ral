//! Calls, argv boundaries and handler arms.

use super::Generator;
use crate::ir::{
    Argv, CommandName, CommandWord, Comp, CompKind, Exec, Redirects, ThunkNode, Val, ValListElem,
};
use crate::source::{Span, Spanned, WithSpan};
use crate::ty::{CompTy, CompTyNode, Scheme, Ty, TyNode};
use crate::typecheck::builtins::{Arm, BuiltinDiagnostic, Decl, Manifest};
use crate::typecheck::env::{Inferring, Membrane};
use crate::typecheck::error::{Reason, SpreadHead, TypeErrorKind};
use crate::typecheck::readback::{self, exec_node};
use crate::typecheck::solve::{ArgvBoundary, CallHole};
use crate::typecheck::unify::WeakSource;
use std::sync::Arc;

type BuiltinCall = (CompTy, Vec<Spanned<Val<Inferring>>>, Option<(Ty, usize)>);

/// Literal blocks last, so each is checked against what its siblings told.
fn argument_order<'v>(args: impl IntoIterator<Item = &'v Spanned<Val>>) -> Vec<usize> {
    let mut order: Vec<_> = args
        .into_iter()
        .map(|arg| matches!(arg.item, Val::Thunk(_)))
        .enumerate()
        .collect();
    order.sort_by_key(|&(_, block)| block);
    order.into_iter().map(|(i, _)| i).collect()
}

/// Nodes generated in argument order, put back in source order.
fn in_source_order<T>(mut generated: Vec<(usize, T)>) -> Vec<T> {
    generated.sort_by_key(|&(i, _)| i);
    generated.into_iter().map(|(_, node)| node).collect()
}

enum HeadClass<'m> {
    Binding(Arc<Scheme>),
    Value(&'m Decl),
    BaseFrame,
    External,
}

impl Generator {
    pub(in crate::typecheck) fn app(
        &mut self,
        head: &Comp,
        args: &[Spanned<Val>],
    ) -> (CompTy, CompKind<Inferring>) {
        let (head_ty, head_node) = self.with_span(head.span, |this| match &head.item {
            CompKind::Force(value) => {
                let (ty, node) = this.val(value, None);
                let cty = this.forced(value, &ty, Reason::Head { name: None });
                (cty, Spanned::with_span(head.span, CompKind::Force(node)))
            }
            _ => this.comp(head),
        });
        let (cty, call, args) = self.apply_args(head_ty, Self::forced_variable(head), args.iter());
        (cty, Self::pending_app(head_node, call, args))
    }

    fn pending_app(
        head: Comp<Inferring>,
        call: CallHole,
        args: Vec<Spanned<Val<Inferring>>>,
    ) -> CompKind<Inferring> {
        if args.is_empty() {
            head.item
        } else {
            CompKind::App {
                head: Arc::new(head),
                args: args.into(),
                hole: call,
            }
        }
    }

    /// `head`, which the variable `callee` names when it is one, applied to
    /// `args`.  The chain is always left-to-right; only argument generation is
    /// permuted.
    fn apply_args<'v>(
        &mut self,
        mut head: CompTy,
        callee: Option<&str>,
        args: impl IntoIterator<Item = &'v Spanned<Val>>,
    ) -> (CompTy, CallHole, Vec<Spanned<Val<Inferring>>>) {
        let recursive_call = callee.is_some_and(|name| self.env.is_recursive_binding(name));
        let call = self.solver.call();
        let args: Vec<_> = args
            .into_iter()
            .map(|arg| (arg, self.solver.fresh_ty()))
            .collect();
        for (arg, ty) in &args {
            head = self.with_span(arg.span, |this| {
                this.solver
                    .argument(call, &head, callee, ty.clone(), recursive_call)
            });
        }
        let nodes = self.check_arguments(&args);
        (head, call, nodes)
    }

    /// Each argument against its own parameter type.
    fn check_arguments(&mut self, args: &[(&Spanned<Val>, Ty)]) -> Vec<Spanned<Val<Inferring>>> {
        let blocks: Vec<_> = args
            .iter()
            .map(|(arg, ty)| match &arg.item {
                Val::Thunk(body) => {
                    let wanted = self.solver.fresh_comp();
                    self.with_span(arg.span, |this| {
                        this.solver
                            .equate(ty, &Ty::thunk(wanted.clone()), Reason::Argument);
                    });
                    Some((body, wanted))
                }
                _ => None,
            })
            .collect();
        let generated = argument_order(args.iter().map(|&(arg, _)| arg))
            .into_iter()
            .map(|i| {
                let (arg, ty) = &args[i];
                let node = self.with_span(arg.span, |this| {
                    if let Some((body, wanted)) = &blocks[i] {
                        this.demand_block(arg.span, body, wanted)
                    } else {
                        let (found, value) = this.val(&arg.item, arg.span);
                        this.solver.equate(ty, &found, Reason::Argument);
                        Spanned::with_span(arg.span, value)
                    }
                });
                (i, node)
            })
            .collect();
        in_source_order(generated)
    }

    fn demand_block(
        &mut self,
        span: Option<Span>,
        body: &ThunkNode,
        wanted: &CompTy,
    ) -> Spanned<Val<Inferring>> {
        let node = self.scope(|this| this.check(body.shape(), wanted, Reason::Argument));
        Spanned::with_span(span, Val::thunk(Arc::new(node)))
    }

    pub(in crate::typecheck) fn refuse_spread(&mut self, argv: &Argv, head: SpreadHead) {
        if let Some(span) = argv.iter().find_map(|slot| match slot {
            ValListElem::Spread(value) => Some(value.span),
            ValListElem::Single(_) => None,
        }) {
            self.with_span(span, |this| {
                this.solver
                    .diagnose(TypeErrorKind::SpreadIntoApplication { head });
            });
        }
    }

    /// Arguments of a refused call, generated only for their own diagnostics.
    pub(in crate::typecheck) fn infer_refused_args<'v>(
        &mut self,
        args: impl IntoIterator<Item = &'v Spanned<Val>>,
    ) {
        let args: Vec<_> = args.into_iter().collect();
        for i in argument_order(args.iter().copied()) {
            let arg = args[i];
            self.with_span(arg.span, |this| this.val(&arg.item, arg.span));
        }
    }

    /// Only declaration structure is inspected, never an instantiated scheme token.
    pub(in crate::typecheck) fn declared_spine(&mut self, entry: &Decl) -> (Vec<Ty>, CompTy) {
        let declared = self.solver.declared(entry);
        let TyNode::Thunk(body) = declared.node() else {
            unreachable!("builtin declarations have thunk types")
        };
        let mut tail = body.clone();
        let mut params = Vec::new();
        while let CompTyNode::Fun(param, body) = tail.node() {
            params.push(param.clone());
            tail = body.clone();
        }
        (params, tail)
    }

    fn builtin_call(&mut self, entry: &Decl, name: &str, argv: &Argv) -> BuiltinCall {
        let arity = entry.fixed_arity();
        let (mut params, tail) = self.declared_spine(entry);
        let list_last = params
            .last()
            .is_some_and(|ty| matches!(ty.node(), TyNode::List(_)));
        let Some(args) = argv.positional() else {
            self.infer_refused_args(argv.iter().map(ValListElem::slot));
            self.refuse_spread(
                argv,
                SpreadHead::Builtin {
                    name: name.into(),
                    arity,
                    list_last,
                },
            );
            return (self.solver.fresh_comp(), Vec::new(), None);
        };
        if args.len() > arity {
            self.solver.diagnose(match entry.diagnostic {
                BuiltinDiagnostic::Decoder if arity == 0 => {
                    TypeErrorKind::DecoderTakesNoArgument { name: name.into() }
                }
                _ => TypeErrorKind::BuiltinArity {
                    name: name.into(),
                    expected: arity,
                    got: args.len(),
                    list_last,
                },
            });
            self.infer_refused_args(args.iter().copied());
            return (self.solver.fresh_comp(), Vec::new(), None);
        }
        let boundary = self.note_boundary_call(entry, &tail, arity - args.len());
        let rest = params.split_off(args.len());
        let typed: Vec<_> = args.iter().copied().zip(params).collect();
        let nodes = self.check_arguments(&typed);
        (CompTy::arrows(rest, tail), nodes, boundary)
    }

    pub(in crate::typecheck) fn note_boundary_call(
        &mut self,
        entry: &Decl,
        declared_tail: &CompTy,
        missing: usize,
    ) -> Option<(Ty, usize)> {
        if !entry.is_boundary() {
            return None;
        }
        let CompTyNode::Return(_, ty) = declared_tail.node() else {
            return None;
        };
        self.solver
            .weak(ty, WeakSource::Boundary(entry.name.as_ref().into()));
        Some((ty.clone(), missing))
    }

    pub(in crate::typecheck) fn instantiate_comp(&mut self, name: &str, scheme: &Scheme) -> CompTy {
        let ty = self.solver.instantiate(scheme);
        self.forced(
            &Val::Variable(name.into()),
            &ty,
            Reason::Head {
                name: Some(name.into()),
            },
        )
    }

    pub(in crate::typecheck) fn exec(
        &mut self,
        comp: &Comp,
        exec: &Exec,
    ) -> (CompTy, CompKind<Inferring>) {
        let redirects = exec.redirects.map(|operand| self.val(operand, None).1);
        let (cty, node) = match &exec.head {
            CommandWord::Name(CommandName::Bare(name)) => {
                self.exec_comp_ty(comp, name, exec, redirects)
            }
            CommandWord::External(name)
            | CommandWord::Name(name @ (CommandName::Path(_) | CommandName::TildePath(_))) => {
                let (cty, argv) = self.external_exec_comp_ty(&name.written(), &exec.argv, false);
                (cty, exec_node(&exec.head, argv, redirects, None))
            }
        };
        let cty = if exec.redirects.is_empty() {
            cty
        } else {
            let producer = self.force_return_shape(&cty, Reason::RedirectShape);
            if exec.redirects.stdout.is_some() {
                self.produces(producer.ty)
            } else {
                producer.into()
            }
        };
        (cty, node)
    }

    fn exec_comp_ty(
        &mut self,
        comp: &Comp,
        name: &str,
        exec: &Exec,
        redirects: Redirects<Val<Inferring>>,
    ) -> (CompTy, CompKind<Inferring>) {
        // Rows are borrowed, not cloned: a cloned row's scheme cache dies with it.
        let builtins = self.env.builtins.clone();
        match self.head_class(&builtins, name) {
            HeadClass::Binding(scheme) => {
                let ty = self.solver.instantiate(&scheme);
                let imported = self.note_residual_use(name, &scheme, &ty);
                let head_ty = self.forced(
                    &Val::Variable(name.into()),
                    &ty,
                    Reason::Head {
                        name: Some(name.into()),
                    },
                );
                let Some(args) = exec.argv.positional() else {
                    self.infer_refused_args(exec.argv.iter().map(ValListElem::slot));
                    self.refuse_spread(&exec.argv, SpreadHead::Applied);
                    return (self.solver.fresh_comp(), CompKind::Return(Val::Unit));
                };
                let (cty, call, args) = self.apply_args(head_ty, Some(name), args);
                let node = if imported {
                    let head = Spanned::with_span(
                        comp.span,
                        CompKind::Force(Val::Protected {
                            value: Box::new(Val::Variable(name.into())),
                            // A forced head is a thunk, so readback keeps this membrane.
                            site: Membrane::Imported(ty),
                        }),
                    );
                    let call = Self::pending_app(head, call, args);
                    readback::redirected(Spanned::with_span(comp.span, call), redirects)
                } else {
                    CompKind::BindingCall {
                        head: name.into(),
                        args: args.into(),
                        redirects,
                        hole: call,
                    }
                };
                (cty, node)
            }
            HeadClass::Value(entry) => {
                let (cty, args, boundary) = self.builtin_call(entry, name, &exec.argv);
                let (site, missing) = match boundary {
                    Some((ty, missing)) => (Some(ty), missing),
                    None => (None, 0),
                };
                let mut argv: Argv<Inferring> = args.into_iter().map(ValListElem::Single).collect();
                if missing > 0 {
                    let (params, extra) = readback::eta(&mut self.synth_counter, missing);
                    argv.extend(extra.into_iter().map(ValListElem::Single));
                    let call = exec_node(&exec.head, argv, redirects, site);
                    (cty, readback::lambda_comp(comp.span, params, call).item)
                } else {
                    (cty, exec_node(&exec.head, argv, redirects, site))
                }
            }
            HeadClass::BaseFrame => {
                let argv = self.argv(&exec.argv, &ArgvBoundary::InShell(name.into()));
                (
                    CompTy::command(),
                    exec_node(&exec.head, argv, redirects, None),
                )
            }
            HeadClass::External => {
                let (cty, argv) =
                    self.external_exec_comp_ty(name, &exec.argv, self.env.is_handled(name));
                (cty, exec_node(&exec.head, argv, redirects, None))
            }
        }
    }

    fn head_class<'m>(&self, builtins: &'m Manifest, name: &str) -> HeadClass<'m> {
        if let Some(scheme) = self.env.lookup_binding(name) {
            HeadClass::Binding(Arc::clone(scheme))
        } else if let Some(entry) = builtins.value(name) {
            HeadClass::Value(entry)
        } else if builtins.base_frame(name).is_some() {
            HeadClass::BaseFrame
        } else {
            HeadClass::External
        }
    }

    fn external_exec_comp_ty(
        &mut self,
        shown: &str,
        argv: &Argv,
        handled: bool,
    ) -> (CompTy, Argv<Inferring>) {
        (
            CompTy::command(),
            self.argv(
                argv,
                &ArgvBoundary::Exec {
                    command: shown.into(),
                    handled,
                },
            ),
        )
    }

    pub(in crate::typecheck) fn argv(
        &mut self,
        argv: &Argv,
        boundary: &ArgvBoundary,
    ) -> Argv<Inferring> {
        let generated = argument_order(argv.iter().map(ValListElem::slot))
            .into_iter()
            .map(|i| {
                let arg = argv[i].slot();
                let node = self.with_span(arg.span, |this| {
                    let (ty, value) = this.val(&arg.item, arg.span);
                    let node = Spanned::with_span(arg.span, value);
                    match &argv[i] {
                        ValListElem::Single(_) => {
                            this.solver.gate(&ty, boundary.clone());
                            ValListElem::Single(node)
                        }
                        ValListElem::Spread(_) => {
                            let elem = this.solver.fresh_ty();
                            this.solver.equate(&ty, &Ty::list(elem), Reason::ListSpread);
                            ValListElem::Spread(node)
                        }
                    }
                });
                (i, node)
            })
            .collect();
        in_source_order(generated).into()
    }

    /// Like `check`, retaining the shape equation's verdict to avoid a second refusal.
    fn check_arm_body(
        &mut self,
        comp: &Comp,
        wanted: &CompTy,
        why: Reason,
    ) -> (bool, Comp<Inferring>) {
        self.with_span(comp.span, |this| {
            let (met, item) = if let CompKind::Lam { param, body } = &comp.item {
                let ty = this.solver.fresh_ty();
                let result = this.solver.fresh_comp();
                let shape = CompTy::arrows([ty.clone()], result.clone());
                let met = this.solver.equate_comp(wanted, &shape, why.clone());
                let (body_met, body) = this.scope(|this| {
                    this.env.bind(param.to_string(), Scheme::mono(ty));
                    this.check_arm_body(body, &result, why)
                });
                (
                    met && body_met,
                    CompKind::Lam {
                        param: param.clone(),
                        body: Arc::new(body),
                    },
                )
            } else {
                let (found, node) = this.comp(comp);
                let met = this.solver.equate_comp(wanted, &found, why);
                (met, node.item)
            };
            (met, Spanned::with_span(comp.span, item))
        })
    }

    pub(in crate::typecheck) fn arm(
        &mut self,
        block: &Val,
        arm: Arm<'_>,
        at: Option<Span>,
    ) -> (CompTy, Val<Inferring>) {
        self.with_span(at, |this| {
            let value = this.solver.fresh_ty();
            let producer = this.produces(value);
            let wanted = arm.shape(producer);
            let why = Reason::HandlerParams(arm.standing());
            let (found, node) = match block {
                Val::Thunk(body) => {
                    let (met, node) =
                        this.scope(|this| this.check_arm_body(body.shape(), &wanted, why.clone()));
                    let found = if met {
                        Ty::thunk(wanted)
                    } else {
                        this.solver.fresh_ty()
                    };
                    (found, Val::thunk(Arc::new(node)))
                }
                held => this.val(held, at),
            };
            (this.stands_in(&found, arm), node)
        })
    }

    pub(in crate::typecheck) fn install_arm(
        &mut self,
        block: &Val,
        name: &str,
        at: Option<Span>,
    ) -> Val<Inferring> {
        self.with_span(at, |this| {
            this.region(|this| this.arm(block, Arm::Named(name), at).1)
        })
    }

    pub(in crate::typecheck) fn stands_in(&mut self, found: &Ty, arm: Arm<'_>) -> CompTy {
        self.solver.stands_in(found, arm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::TildePath;
    use crate::source::FileId;
    use crate::ty::Grade;
    use crate::typecheck::builtins::{Convention, scheme};
    use crate::typecheck::generate::tests::{assert_solved_comp, solved};
    use crate::typecheck::solve::Solver;
    use std::borrow::Cow;

    fn generator() -> Generator {
        Generator::new(Solver::new(), super::super::TyEnv::new(), Vec::new())
    }

    fn at(start: u32) -> Span {
        Span::new(FileId::DUMMY, start, start + 1)
    }

    fn home(start: u32) -> Comp {
        Spanned::new(at(start), CompKind::Tilde(TildePath { suffix: None }))
    }

    fn block(start: u32) -> Spanned<Val> {
        Spanned::new(at(start), Val::thunk(Arc::new(home(start))))
    }

    fn bare(name: &str, argv: Argv) -> Comp {
        Spanned::new(
            at(0),
            exec_node(
                &CommandWord::Name(CommandName::Bare(name.into())),
                argv,
                Redirects::default(),
                None,
            ),
        )
    }

    #[test]
    fn argument_order_defers_literal_blocks_and_preserves_source_slots() {
        let args = [
            block(1),
            Spanned::new(at(2), Val::Int(2)),
            block(3),
            Spanned::new(at(4), Val::Int(4)),
        ];
        assert_eq!(argument_order(&args), [1, 3, 0, 2]);
    }

    #[test]
    fn apply_args_waits_for_context_and_keeps_the_chain_left_to_right() {
        let mut generator = generator();
        let head = generator.solver.fresh_comp();
        let args = [block(1), block(2)];
        let (result, call, nodes) = generator.apply_args(head.clone(), None, &args);
        let param = Ty::thunk(CompTy::pure(Ty::string()));
        let inner = CompTy::arrows([param.clone()], CompTy::pure(Ty::int()));
        let middle = CompTy::pure(Ty::thunk(inner));
        let first = CompTy::arrows([param], middle);
        let context = CompTy::pure(Ty::thunk(first));
        generator
            .solver
            .equate_comp(&head, &context, Reason::Argument);
        assert_eq!(
            nodes.iter().map(|node| node.span).collect::<Vec<_>>(),
            [Some(at(1)), Some(at(2))]
        );
        let solution = solved(generator.solver);
        assert_eq!(solution.resolve_comp(&result), CompTy::pure(Ty::int()));
        assert_eq!(solution.splits(call), [0, 1]);
    }

    #[test]
    fn app_carries_its_call_hole_even_when_its_head_is_a_lambda() {
        let mut generator = generator();
        let head = Spanned::new(
            at(1),
            CompKind::Lam {
                param: "arg".into(),
                body: Arc::new(home(2)),
            },
        );
        let (cty, item) = generator.app(&head, &[block(3)]);
        let CompKind::App { head, args, hole } = item else {
            panic!("application preserved")
        };
        assert_eq!(args.len(), 1);
        assert!(matches!(head.item, CompKind::Lam { .. }));
        let solution = solved(generator.solver);
        assert_eq!(solution.splits(hole), Vec::<usize>::new());
        assert!(
            matches!(solution.resolve_comp(&cty).node(), CompTyNode::Return(_, ty) if *ty == Ty::string())
        );
    }

    #[test]
    fn imported_binding_app_keeps_both_the_admission_and_the_call() {
        let stored = Arc::new(Scheme::mono(Ty::thunk(CompTy::arrows(
            [Ty::thunk(CompTy::pure(Ty::string()))],
            CompTy::pure(Ty::int()),
        ))));
        let mut generator = Generator::new(
            Solver::new(),
            super::super::TyEnv::new(),
            vec![("f".into(), Arc::clone(&stored))],
        );
        generator.env.bind("f".into(), stored);
        let source = bare("f", vec![ValListElem::Single(block(1))].into());
        let (_, node) = generator.comp(&source);
        let CompKind::App { head, .. } = node.item else {
            panic!("imported binding is forced under admission")
        };
        assert!(matches!(
            head.item,
            CompKind::Force(Val::Protected {
                site: Membrane::Imported(_),
                ..
            })
        ));
        assert_eq!(generator.residual_uses.len(), 1);
        solved(generator.solver);
    }

    #[test]
    fn overapplied_builtins_refuse_parameter_checks_and_keep_declared_list_help() {
        for (source, list_last) in [("intercalate ', ' a b c", true), ("upper 1 2", false)] {
            let errors = crate::compile::compile_and_typecheck(
                source,
                crate::test_helper::core_schemes(),
                FileId::DUMMY,
                None,
                None,
            )
            .expect_err("overapplied builtin");
            let crate::compile::CompileError::Types(errors) = errors else {
                panic!("{source}: {errors:?}")
            };
            assert_eq!(
                errors
                    .iter()
                    .map(|error| error.kind.code())
                    .collect::<Vec<_>>(),
                ["T0050"],
                "{source}: {errors:?}"
            );
            assert_eq!(
                errors[0]
                    .hint()
                    .unwrap()
                    .contains("write the arguments from the 2nd on as one list"),
                list_last,
                "{source}: {errors:?}"
            );
        }
    }

    #[test]
    fn partial_boundary_exec_eta_expands_from_its_declared_spine_and_keeps_a_weak_result() {
        let mut generator = generator();
        let entry = Decl::new(
            Cow::Borrowed("decode-at"),
            Convention::Value(scheme::from_json_at),
            "",
            true,
        );
        generator.env.builtins.push(vec![entry].into());
        let (cty, node) = generator.region(|this| this.comp(&bare("decode-at", Argv::default())));
        let CompKind::Lam { body, .. } = node.item else {
            panic!("partial boundary is eta expanded")
        };
        let CompKind::Exec(exec) = &body.item else {
            panic!("boundary remains a direct builtin exec")
        };
        assert_eq!(exec.argv.len(), 1);
        let Some(result) = exec.site.clone() else {
            panic!("boundary result site")
        };
        let stored = generator.solver.generalise(&Ty::thunk(cty));
        let first = generator.instantiate_comp("first", &stored);
        let second = generator.instantiate_comp("second", &stored);
        generator.solver.equate_comp(
            &first,
            &CompTy::arrows([Ty::argv()], CompTy::pure(Ty::int())),
            Reason::Argument,
        );
        assert!(!generator.solver.equate_comp(
            &second,
            &CompTy::arrows([Ty::argv()], CompTy::pure(Ty::bool())),
            Reason::Argument
        ));
        let (store, errors) = generator.solver.refused();
        assert_eq!(errors.len(), 1);
        assert_eq!(store.resolve_ty(&result), Ty::int());
    }

    #[test]
    fn arm_checks_its_slot_before_standing_in_and_installs_at_the_head_after_refusal() {
        let mut generator = generator();
        // This nullary block is refused at the slot, not also for its return value.
        let (installed, _) = generator.arm(
            &Val::thunk(Arc::new(home(1))),
            Arm::Named("tool"),
            Some(at(0)),
        );
        let (store, errors) = generator.solver.refused();
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            errors[0].reason,
            Some(Reason::HandlerParams(crate::typecheck::Standing::Command(
                _
            )))
        ));
        assert_solved_comp(
            &store,
            &installed,
            &CompTy::arrows([Ty::argv()], CompTy::command()),
        );
    }

    #[test]
    fn a_base_frame_is_typed_as_a_handler_for_its_name() {
        let manifest = crate::HostSurface::default().manifest();
        for entry in manifest.base_frames() {
            let expected = Arm::Named(&entry.name).ty();
            assert_eq!(entry.scheme().ty, Ty::thunk(expected));
            let mut generator = generator();
            generator.env.builtins = manifest.clone();
            let source = bare(&entry.name, Argv::default());
            let (result, _) = generator.comp(&source);
            assert_eq!(
                solved(generator.solver).resolve_comp(&result),
                CompTy::command()
            );
        }
    }

    #[test]
    fn catch_all_slot_takes_name_then_argv_and_stands_for_a_command() {
        let mut generator = generator();
        let block = Val::thunk(Arc::new(Spanned::new(
            at(1),
            CompKind::Lam {
                param: "name".into(),
                body: Arc::new(Spanned::new(
                    at(2),
                    CompKind::Lam {
                        param: "args".into(),
                        body: Arc::new(bare("tool", Argv::default())),
                    },
                )),
            },
        )));
        let (installed, _) = generator.arm(&block, Arm::CatchAll, Some(at(0)));
        assert_solved_comp(
            &solved(generator.solver).scratch(),
            &installed,
            &CompTy::arrows(
                [Ty::string(), Ty::argv()],
                CompTy::ret(Grade::Output, Ty::unit()),
            ),
        );
    }

    #[test]
    fn handled_external_keeps_its_gate_and_hint_after_scope_exit() {
        let mut generator = generator();
        generator.scope(|this| {
            this.env.bind_handler("tool".into());
            this.statement(&bare("tool", vec![ValListElem::Single(block(1))].into()));
        });
        assert!(!generator.env.is_handled("tool"));
        let errors = generator
            .solver
            .close()
            .err()
            .expect("external gate refusal");
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            errors[0].kind,
            TypeErrorKind::ExecArgNotText { handled: true, .. }
        ));
        assert!(
            errors[0]
                .hint()
                .unwrap()
                .contains("`tool` has a handler here")
        );
    }
    #[test]
    fn forced_and_path_heads_never_report_a_handler() {
        let heads = [
            CommandWord::External(CommandName::Bare("tool".into())),
            CommandWord::Name(CommandName::Path("./tool".into())),
            CommandWord::Name(CommandName::TildePath(TildePath {
                suffix: Some("/tool".into()),
            })),
        ];
        for head in heads {
            let mut generator = generator();
            for name in ["tool", "./tool", "~/tool"] {
                generator.env.bind_handler(name.into());
            }
            let source = Spanned::new(
                at(0),
                exec_node(
                    &head,
                    vec![ValListElem::Single(block(1))].into(),
                    Redirects::default(),
                    None,
                ),
            );
            generator.statement(&source);
            let errors = generator
                .solver
                .close()
                .err()
                .expect("external gate refusal");
            assert_eq!(errors.len(), 1);
            assert!(matches!(
                errors[0].kind,
                TypeErrorKind::ExecArgNotText { handled: false, .. }
            ));
            assert!(!errors[0].hint().unwrap().contains("has a handler here"));
        }
    }

    #[test]
    fn lexical_functions_shadow_value_builtins_and_base_frames() {
        for name in ["upper", "echo"] {
            let source =
                format!("let {name} = {{ |x| return 41 }}; let n = {name} 'x'; return $[$n + 1]");
            crate::compile::compile_and_typecheck(
                &source,
                crate::test_helper::core_schemes(),
                FileId::DUMMY,
                None,
                None,
            )
            .unwrap_or_else(|errors| panic!("{source}: {errors:?}"));
        }
    }

    #[test]
    fn installing_handlers_does_not_change_command_result_types() {
        for name in ["echo", "tool"] {
            for prefix in [
                String::new(),
                format!("within [handlers: [:, {name}: {{ |args| echo ...$args }}]] "),
            ] {
                let source = format!("{prefix}{{ let n = {name} 'x'; return $[$n + 1] }}");
                let errors = crate::compile::compile_and_typecheck(
                    &source,
                    crate::test_helper::core_schemes(),
                    FileId::DUMMY,
                    None,
                    None,
                )
                .expect_err("command output remains text, not an integer");
                let crate::compile::CompileError::Types(errors) = errors else {
                    panic!("{source}: {errors:?}")
                };
                assert!(
                    errors.iter().any(|error| error.kind.code() == "T0074"),
                    "{source}: {errors:?}"
                );
            }
        }
    }

    #[test]
    fn a_session_head_must_be_a_thunk() {
        let bound = |ty: Option<Ty>| {
            let scheme = ty.map(|ty| crate::ty::Binders::close(|_| ty));
            vec![("date".to_string(), scheme)]
        };
        assert_eq!(
            super::super::tests::error_codes_seeded("date +%s", bound(Some(Ty::int()))),
            ["T0072"]
        );
        assert_eq!(
            super::super::tests::error_codes_seeded("date +%s", bound(None)),
            Vec::<&str>::new()
        );
    }
}
