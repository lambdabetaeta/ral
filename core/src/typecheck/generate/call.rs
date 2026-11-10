//! Calls, argv boundaries and handler arms.

use super::Generator;
use crate::data::Str;
use crate::ir::{
    Argv, CommandName, CommandWord, Comp, CompKind, Exec, Redirects, ThunkNode, Val, ValListElem,
};
use crate::source::{Span, Spanned, WithSpan};
use crate::ty::{CompTy, Scheme, Ty};
use crate::typecheck::builtins::{BuiltinDiagnostic, Decl, Manifest};
use crate::typecheck::contract::catch_all_shape;
use crate::typecheck::env::{HandlerBinding, HandlerOrigin, Inferring, Membrane};
use crate::typecheck::error::{Reason, SpreadHead, Standing, TypeErrorKind};
use crate::typecheck::readback::{self, exec_node};
use crate::typecheck::solve::{ArgvBoundary, CallHole};
use crate::typecheck::unify::WeakSource;
use std::sync::Arc;

#[derive(Clone, Copy)]
pub(in crate::typecheck) enum Arm<'a> {
    Named(&'a str),
    CatchAll,
}

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
    Arm(HandlerBinding),
    External,
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

impl Generator {
    pub(in crate::typecheck) fn handler_statement(
        &mut self,
        comp: &Comp,
    ) -> Option<CompKind<Inferring>> {
        self.with_span(comp.span, |this| match HandlerStatement::of(comp) {
            Ok(Some(HandlerStatement::Alias { head, name, block })) => {
                let (scheme, arm) = this.install_arm(&block.item, name.item.as_str(), block.span);
                this.env
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
                this.env.unbind_removable_handler(name.item.as_str());
                let argv = vec![ValListElem::Single(Spanned::with_span(
                    name.span,
                    Val::String(name.item.clone()),
                ))]
                .into();
                Some(exec_node(head, argv, Redirects::default(), None))
            }
            Ok(None) => None,
            Err(kind) => {
                this.solver.diagnose(kind);
                Some(CompKind::Return(Val::Unit))
            }
        })
    }

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
                        this.solver.equate(
                            ty,
                            &Ty::Thunk(Box::new(wanted.clone())),
                            Reason::Argument,
                        );
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
        let Ty::Thunk(body) = self.solver.declared(entry) else {
            unreachable!("builtin declarations have thunk types")
        };
        let mut tail = *body;
        let mut params = Vec::new();
        while let CompTy::Fun(param, body) = tail {
            params.push(*param);
            tail = *body;
        }
        (params, tail)
    }

    fn builtin_call(&mut self, entry: &Decl, name: &str, argv: &Argv) -> BuiltinCall {
        let arity = entry.fixed_arity();
        let Some(args) = argv.positional() else {
            self.infer_refused_args(argv.iter().map(ValListElem::slot));
            self.refuse_spread(
                argv,
                SpreadHead::Builtin {
                    name: name.into(),
                    arity,
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
                },
            });
        }
        let (mut params, tail) = self.declared_spine(entry);
        let boundary = self.note_boundary_call(entry, &tail, arity.saturating_sub(args.len()));
        let rest = params.split_off(args.len().min(params.len()));
        let (typed, surplus) = args.split_at(params.len());
        let typed: Vec<_> = typed.iter().copied().zip(params).collect();
        let nodes = self.check_arguments(&typed);
        self.infer_refused_args(surplus.iter().copied());
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
        let CompTy::Return(_, ty) = declared_tail else {
            return None;
        };
        self.solver
            .weak(ty, WeakSource::Boundary(entry.name.as_ref().into()));
        Some((ty.as_ref().clone(), missing))
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
                let (cty, argv) = self.external_exec_comp_ty(&name.written(), &exec.argv);
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
            HeadClass::Arm(handler) => {
                let (cty, argv) = self.apply_alias_arm(name, &handler.scheme, &exec.argv);
                (cty, exec_node(&exec.head, argv, redirects, None))
            }
            HeadClass::External => {
                let (cty, argv) = self.external_exec_comp_ty(name, &exec.argv);
                (cty, exec_node(&exec.head, argv, redirects, None))
            }
        }
    }

    fn head_class<'m>(&self, builtins: &'m Manifest, name: &str) -> HeadClass<'m> {
        if let Some(scheme) = self.env.lookup_binding(name) {
            HeadClass::Binding(Arc::clone(scheme))
        } else if let Some(entry) = builtins.value(name) {
            HeadClass::Value(entry)
        } else if let Some(handler) = self.env.lookup_handler(name) {
            HeadClass::Arm(handler.clone())
        } else {
            HeadClass::External
        }
    }

    fn apply_alias_arm(
        &mut self,
        name: &str,
        scheme: &Scheme,
        argv: &Argv,
    ) -> (CompTy, Argv<Inferring>) {
        let head = self.instantiate_comp(name, scheme);
        let result = self.solver.fresh_comp();
        self.solver.equate_comp(
            &head,
            &CompTy::arrows([Ty::argv()], result.clone()),
            Reason::AliasParam,
        );
        let argv = self.argv(argv, ArgvBoundary::InShell(name.into()));
        (result, argv)
    }

    fn external_exec_comp_ty(&mut self, shown: &str, argv: &Argv) -> (CompTy, Argv<Inferring>) {
        (
            CompTy::command(),
            self.argv(argv, ArgvBoundary::Exec(shown.into())),
        )
    }

    pub(in crate::typecheck) fn argv(
        &mut self,
        argv: &Argv,
        boundary: ArgvBoundary,
    ) -> Argv<Inferring> {
        let (command, external) = match boundary {
            ArgvBoundary::Exec(command) => (command, true),
            ArgvBoundary::InShell(command) => (command, false),
        };
        let generated = argument_order(argv.iter().map(ValListElem::slot))
            .into_iter()
            .map(|i| {
                let arg = argv[i].slot();
                let node = self.with_span(arg.span, |this| {
                    let (ty, value) = this.val(&arg.item, arg.span);
                    let node = Spanned::with_span(arg.span, value);
                    match &argv[i] {
                        ValListElem::Single(_) => {
                            let boundary = if external {
                                ArgvBoundary::Exec(command.clone())
                            } else {
                                ArgvBoundary::InShell(command.clone())
                            };
                            this.solver.gate(&ty, boundary);
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
            let (wanted, why) = match arm {
                Arm::Named(_) => (
                    CompTy::arrows([Ty::argv()], producer.clone()),
                    Reason::AliasParam,
                ),
                Arm::CatchAll => (catch_all_shape(producer.clone()), Reason::CatchAllParams),
            };
            let (met, node) = match block {
                Val::Thunk(body) => {
                    let (met, node) =
                        this.scope(|this| this.check_arm_body(body.shape(), &wanted, why.clone()));
                    (met, Val::thunk(Arc::new(node)))
                }
                held => {
                    let (ty, node) = this.val(held, at);
                    let got = this.solver.fresh_comp();
                    let thunk = this.solver.equate(
                        &ty,
                        &Ty::Thunk(Box::new(got.clone())),
                        Reason::HandlerArm,
                    );
                    let met = thunk && this.solver.equate_comp(&wanted, &got, why);
                    (met, node)
                }
            };
            let (head, standing) = this.stands_for(arm);
            let value = this.solver.fresh_ty();
            let head_producer = this.produces(value);
            this.solver.equate_comp(
                &head,
                &CompTy::arrows([Ty::argv()], head_producer.clone()),
                Reason::ReturnShape,
            );
            if met {
                this.solver
                    .equate_comp(&head_producer, &producer, Reason::StandsIn(standing));
            }
            let installed = match arm {
                Arm::Named(_) => CompTy::arrows([Ty::argv()], head_producer),
                Arm::CatchAll => catch_all_shape(head_producer),
            };
            (installed, node)
        })
    }

    pub(in crate::typecheck) fn install_arm(
        &mut self,
        block: &Val,
        name: &str,
        at: Option<Span>,
    ) -> (Scheme, Val<Inferring>) {
        let (cty, node) = self.with_span(at, |this| {
            this.region(|this| this.arm(block, Arm::Named(name), at))
        });
        (self.solver.generalise(&Ty::Thunk(Box::new(cty))), node)
    }

    fn stands_for(&mut self, arm: Arm<'_>) -> (CompTy, Standing) {
        match arm {
            Arm::Named(name) => match self.env.lookup_handler(name).cloned() {
                Some(handler) => (
                    self.instantiate_comp(name, &handler.scheme),
                    Standing::Own(name.into()),
                ),
                None => (
                    CompTy::arrows([Ty::argv()], CompTy::command()),
                    Standing::Command(name.into()),
                ),
            },
            Arm::CatchAll => (
                CompTy::arrows([Ty::argv()], CompTy::command()),
                Standing::EveryCommand,
            ),
        }
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
        let param = Ty::Thunk(Box::new(CompTy::pure(Ty::String)));
        let inner = CompTy::arrows([param.clone()], CompTy::pure(Ty::Int));
        let middle = CompTy::pure(Ty::Thunk(Box::new(inner)));
        let first = CompTy::arrows([param], middle);
        let context = CompTy::pure(Ty::Thunk(Box::new(first)));
        generator
            .solver
            .equate_comp(&head, &context, Reason::Argument);
        assert_eq!(
            nodes.iter().map(|node| node.span).collect::<Vec<_>>(),
            [Some(at(1)), Some(at(2))]
        );
        let solution = solved(generator.solver);
        assert_eq!(solution.resolve_comp(&result), CompTy::pure(Ty::Int));
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
        assert!(matches!(solution.resolve_comp(&cty), CompTy::Return(_, ty) if *ty == Ty::String));
    }

    #[test]
    fn bare_binding_call_carries_its_hole_even_without_arguments() {
        let mut generator = generator();
        generator.env.bind(
            "f".into(),
            Scheme::mono(Ty::Thunk(Box::new(CompTy::command()))),
        );
        let source = bare("f", Argv::default());
        let (_, node) = generator.comp(&source);
        let CompKind::BindingCall {
            head, args, hole, ..
        } = node.item
        else {
            panic!("bare call preserved")
        };
        assert_eq!(head.as_ref(), "f");
        assert!(args.is_empty());
        assert_eq!(solved(generator.solver).splits(hole), Vec::<usize>::new());
    }

    #[test]
    fn imported_binding_app_keeps_both_the_admission_and_the_call() {
        let stored = Arc::new(Scheme::mono(Ty::Thunk(Box::new(CompTy::arrows(
            [Ty::Thunk(Box::new(CompTy::pure(Ty::String)))],
            CompTy::pure(Ty::Int),
        )))));
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
    fn partial_boundary_exec_eta_expands_from_its_declared_spine_and_keeps_a_weak_result() {
        let mut generator = generator();
        let entry = Decl::new(
            Cow::Borrowed("decode-at"),
            Convention::Value,
            scheme::from_json_at,
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
        let stored = generator.solver.generalise(&Ty::Thunk(Box::new(cty)));
        let first = generator.instantiate_comp("first", &stored);
        let second = generator.instantiate_comp("second", &stored);
        generator.solver.equate_comp(
            &first,
            &CompTy::arrows([Ty::argv()], CompTy::pure(Ty::Int)),
            Reason::Argument,
        );
        assert!(!generator.solver.equate_comp(
            &second,
            &CompTy::arrows([Ty::argv()], CompTy::pure(Ty::Bool)),
            Reason::Argument
        ));
        let (store, errors) = generator.solver.refused();
        assert_eq!(errors.len(), 1);
        assert_eq!(store.resolve_ty(&result), Ty::Int);
    }

    #[test]
    fn malformed_handler_statement_reports_once_without_touching_the_environment() {
        let mut generator = generator();
        let source = bare("alias", Argv::default());
        assert!(matches!(
            generator.handler_statement(&source),
            Some(CompKind::Return(Val::Unit))
        ));
        let errors = generator.solver.refused().1;
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            errors[0].kind,
            TypeErrorKind::MalformedAlias { .. }
        ));
        assert_eq!(errors[0].pos, Some(at(0)));
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
        assert!(matches!(errors[0].reason, Some(Reason::AliasParam)));
        assert_solved_comp(
            &store,
            &installed,
            &CompTy::arrows([Ty::argv()], CompTy::command()),
        );
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
                [Ty::String, Ty::argv()],
                CompTy::Return(Grade::Output, Box::new(Ty::Unit)),
            ),
        );
    }

    #[test]
    fn install_arm_generalises_only_after_its_body_region_closes() {
        use crate::ty::{Binders, Kind};
        use crate::typecheck::builtins::thunk;

        let mut generator = generator();
        let original = Binders::close(|binders| {
            let a = Ty::Var(binders.ty(Kind::ANY));
            thunk(CompTy::arrows([Ty::argv()], CompTy::pure(Ty::list(a))))
        });
        generator.env.bind_handler(
            "items".into(),
            Arc::clone(original.scheme()),
            HandlerOrigin::Within,
        );
        let block = Val::thunk(Arc::new(Spanned::new(
            at(1),
            CompKind::Lam {
                param: "args".into(),
                body: Arc::new(Spanned::new(at(2), CompKind::Return(Val::list(Vec::new())))),
            },
        )));
        let (stored, _) = generator.install_arm(&block, "items", Some(at(0)));
        for elem in [Ty::Int, Ty::Bool] {
            let instance = generator.instantiate_comp("items", &stored);
            assert!(generator.solver.equate_comp(
                &instance,
                &CompTy::arrows([Ty::argv()], CompTy::pure(Ty::list(elem))),
                Reason::Argument,
            ));
        }
        solved(generator.solver);
    }

    #[test]
    fn handler_statements_install_and_remove_only_aliases() {
        let mut generator = generator();
        let arm = Val::thunk(Arc::new(Spanned::new(
            at(2),
            CompKind::Lam {
                param: "args".into(),
                body: Arc::new(bare("tool", Argv::default())),
            },
        )));
        let alias = bare(
            "alias",
            vec![
                ValListElem::Single(Spanned::new(at(1), Val::String("tool".into()))),
                ValListElem::Single(Spanned::new(at(2), arm)),
            ]
            .into(),
        );
        assert!(matches!(
            generator.handler_statement(&alias),
            Some(CompKind::Exec(_))
        ));
        assert_eq!(
            generator
                .env
                .lookup_handler("tool")
                .map(|binding| binding.origin == HandlerOrigin::Alias),
            Some(true)
        );
        let unalias = bare(
            "unalias",
            vec![ValListElem::Single(Spanned::new(
                at(3),
                Val::String("tool".into()),
            ))]
            .into(),
        );
        assert!(matches!(
            generator.handler_statement(&unalias),
            Some(CompKind::Exec(_))
        ));
        assert!(generator.env.lookup_handler("tool").is_none());
        solved(generator.solver);
    }
    #[test]
    fn a_session_head_must_be_a_thunk() {
        let bound = |ty: Option<Ty>| {
            let scheme = ty.map(|ty| crate::ty::Binders::close(|_| ty));
            vec![("date".to_string(), scheme)]
        };
        assert_eq!(
            super::super::tests::error_codes_seeded("date +%s", bound(Some(Ty::Int))),
            ["T0072"]
        );
        assert_eq!(
            super::super::tests::error_codes_seeded("date +%s", bound(None)),
            Vec::<&str>::new()
        );
    }
}
