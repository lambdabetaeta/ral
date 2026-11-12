//! CBPV closures step; values settle immediately through restoring frames.

use crate::fact::{
    ApplyFailure, ArityFailure, CallOrForce, CaptureLimit, CaseArmFailure, CountRule, DepthFailure,
    DepthKind, ErrorFact, EvalFailure, ExitCase, Expected, InvalidCause, InvariantFailure, Reason,
    StateFailure, ValueShape, tag,
};
use crate::shell::runtime::invalid;
use either::Either;
use std::borrow::Cow;
use std::ops::ControlFlow;
use std::sync::Arc;

use crate::builtin::BuiltinEntry;
use crate::handle::brood::{BroodGuard, BroodOwner, Cradle};
use crate::io::{self, Sink};
use crate::ir::Name;
use crate::ir::Redirects;
use crate::ir::{
    Argv, CaseArm, CommandName, CommandWord, Comp, CompKind, Pattern, Prim, Val, ValListElem,
    is_synthetic,
};
use crate::process::Scope;
use crate::shell::builtins::concurrency::{self, legible_title};
use crate::shell::exec::command::LookupFailure;
use crate::shell::exec::pipeline;
use crate::shell::exec::redirect::RedirectState;
use crate::shell::exec::resolve::Resolution;
use crate::shell::grant::decode_capability_map;
use crate::shell::runtime::{
    Binding, Break, CapturePolicy, DisplacedSite, Env, Error, Escape, FrameHandle, FrameName,
    HandlerArity, HandlerFrame, HandlerLookup, Mooring, Settled, Shell, Signature, TrailScope,
    ValClos, View, report_value,
};
use crate::source::{Span, Spanned};
use crate::ty::Site;

use super::assemble::spread_type_err;
use super::expr;
use super::pattern;
use super::scope::{WithinScope, WithinUndo};

// ── The stack ─────────────────────────────────────────────────────────

/// ⟨M, ρ⟩, or `raise e` as `Err(e)`.
type State = Settled<(Arc<Comp>, Env)>;
/// A next computation closure, or a value for the frame below.
type Next = Either<(Arc<Comp>, Env), ValClos>;

const VACANT: State = Err(Break::Escape(Escape::Exit(0)));

fn boxed(t: Next) -> ValClos {
    match t {
        Either::Left((comp, env)) => ValClos::thunk(comp, &env),
        Either::Right(v) => v,
    }
}

fn force(v: ValClos) -> Result<(Arc<Comp>, Env), Error> {
    v.into_computation().map_err(|v| {
        Error::new(Reason::Evaluation(Box::new(EvalFailure::Apply(
            ApplyFailure {
                operation: CallOrForce::Force,
                found: v.found(),
            },
        ))))
    })
}

/// A literal block runs in place, without forming a value closure.
fn force_val(val: &Val, env: Env, sig: &Signature) -> Result<(Arc<Comp>, Env), Error> {
    match val {
        Val::Thunk(node) => Ok((Arc::clone(node.shape()), env)),
        _ => force(ValClos::close(val, &env, sig)?),
    }
}

/// One elimination form with a pending sub-computation. Frames hold `Arc`s
/// into the IR, never cloned IR, never a `Context` clone, and an `Env` only
/// with syntax to close over it: `To` and `Seq`. Large states are boxed.
enum Frame {
    /// Adjacent tail calls share one restoration point.
    CallSite(Box<DisplacedSite>),
    /// `M to x. N`: `bind` is the `Bind` node itself, read for its pattern,
    /// rest and span.
    To {
        bind: Arc<Comp>,
        env: Env,
    },
    Seq {
        rest: Arc<Comp>,
        env: Env,
    },
    /// `□ V`: an argument awaiting a forced function; `span` is its call's.
    Arg {
        value: ValClos,
        span: Option<Span>,
    },
    /// A halt drops `buf`: a capture that does not become its value commits
    /// nothing.
    Capture {
        prev: Sink,
        buf: io::ByteBuffer,
        span: Option<Span>,
    },
    Redirect(Box<RedirectState>),
    Unmask {
        frame: Box<(FrameHandle, HandlerFrame)>,
    },
    Try {
        handler: ValClos,
        span: Option<Span>,
    },
    Acquire {
        body: ValClos,
        release: ValClos,
        prev: Scope,
        span: Option<Span>,
    },
    Guard {
        release: ValClos,
        resource: ValClos,
        span: Option<Span>,
    },
    Cleanup {
        outcome: Settled<ValClos>,
        /// The current scope before the cleanup's shield, restored on pop
        /// and on abandon.
        prev: Scope,
    },
    Within(WithinUndo),
    /// Where its layer sits: a session frame pushed above survives the pop.
    Grant(usize),
    Audit {
        scope: TrailScope,
    },
    /// `gather`: the unique owner of its body's brood, and the cradle to
    /// restore when it exits.
    Gather {
        owner: BroodGuard,
        prev: Cradle,
        span: Option<Span>,
    },
}

/// Large frame states are boxed: any alone would exceed the cap.
const _: () = assert!(std::mem::size_of::<Frame>() <= 128);

/// The configuration ⟨M, ρ | K⟩ under the run's mooring.
pub(crate) struct Machine<'m> {
    state: State,
    stack: Vec<Frame>,
    /// Provenance frames do not consume the computational stack budget.
    call_site_frames: usize,
    mooring: &'m Mooring,
}

impl<'m> Machine<'m> {
    fn new(mooring: &'m Mooring) -> Self {
        Self {
            state: VACANT,
            stack: Vec::new(),
            call_site_frames: 0,
            mooring,
        }
    }
}

/// Nested `machine::evaluate`/`machine::apply` re-entry cap: a
/// native such as `map` applying a user function runs a machine over the
/// *host* stack, so this bounds how deep those nest rather than the
/// machine's own frame count. Calibrated by `nested_machines_fit_a_worker_stack`
/// against a debug build's per-frame cost on a 2 MiB stack, a quarter of a
/// worker's 8 MiB — 256 and 64 both overflowed before tripping the check; 24
/// leaves headroom.
pub(crate) const NESTED_MACHINE_LIMIT: usize = 24;

// ── Small pure helpers ────────────────────────────────────────────────

fn operation_name(comp: &CompKind) -> Option<FrameName> {
    let name = match comp {
        CompKind::Exec(_)
        | CompKind::App { .. }
        | CompKind::Force(_)
        | CompKind::Lam { .. }
        | CompKind::Builtin { .. } => return None,
        CompKind::Prim(Prim::Binary(op, ..)) => op.symbol(),
        &CompKind::BindingCall { hole, .. } => match hole {},
        CompKind::Prim(Prim::Negate(_)) => "-",
        CompKind::Prim(Prim::Not(_)) => "not",
        CompKind::Prim(Prim::Render(_)) => "str",
        CompKind::Prim(Prim::Concat(_)) => "concat",
        CompKind::Prim(Prim::Decode(_)) => "decode",
        CompKind::Prim(Prim::Index { .. }) => "index",
        CompKind::Prim(Prim::Update { .. }) => "update",
        CompKind::Prim(Prim::List(_)) => "list",
        CompKind::Prim(Prim::Map(_)) => "map",
        CompKind::Return(_) => "return",
        CompKind::Bind { .. } => "let",
        CompKind::Seq { .. } => "sequence",
        CompKind::Rec { .. } => "rec",
        CompKind::If { .. } => "if",
        CompKind::Case { .. } => "case",
        CompKind::Try { .. } => "try",
        CompKind::Guard { .. } => "guard",
        CompKind::Capture(_) => "capture",
        CompKind::Pipeline { .. } => "pipeline",
        CompKind::Redirect { .. } => "redirect",
        CompKind::Within { .. } => "within",
        CompKind::Grant { .. } => "grant",
        CompKind::Spawn { .. } => "spawn",
        CompKind::Gather { .. } => "gather",
        CompKind::Audit { .. } => "audit",
        CompKind::Tilde(_) => "~",
        CompKind::Use(_) => "use",
    };
    Some(FrameName::Static(name))
}

/// Compiler binders name no call.
fn forced_name(value: &Val) -> Option<&Name> {
    match value {
        Val::Variable(name) if !is_synthetic(name) => Some(name),
        _ => None,
    }
}

fn exit_case(outcome: &Settled<ValClos>, shell: &Shell) -> ValClos {
    let ending: ExitCase<ErrorFact> = match outcome {
        Ok(_) => ExitCase::Ok,
        Err(Break::Error(error)) => ExitCase::Err(error.fact(shell)),
        Err(Break::Escape(Escape::Exit(code))) => ExitCase::Exited(*code),
        Err(Break::Escape(Escape::Stopped(stop))) => ExitCase::Stopped(stop.cause),
    };
    let (label, payload) = match ending {
        ExitCase::Ok => (tag::OK, ValClos::UNIT),
        ExitCase::Err(fact) => (tag::ERR, ValClos::failure(fact)),
        ExitCase::Exited(code) => (tag::EXITED, ValClos::int(code.into())),
        ExitCase::Stopped(cause) => (tag::STOPPED, ValClos::from_datum(cause)),
    };
    ValClos::variant(label, Some(payload))
}

fn capture_overflowed() -> Error {
    Error::new(Reason::State(Box::new(StateFailure::CaptureLimit(
        CaptureLimit {
            bytes: io::SINK_BUFFER_CAP.into(),
        },
    ))))
}

/// `V` meeting an argument: unreachable for a checked program, which applies
/// only what it has forced; kept a clean error, not a panic.
fn not_a_function(v: &ValClos) -> Error {
    Error::new(Reason::Invariant(Box::new(
        InvariantFailure::AppliedUnforcedValue(v.shape()),
    )))
}

/// `body`, at `span` when it has no span of its own.
fn located(body: &Arc<Comp>, span: Option<Span>) -> Arc<Comp> {
    match (body.span, span) {
        (None, Some(_)) => Arc::new(Spanned::with_span(span, body.item.clone())),
        _ => Arc::clone(body),
    }
}

/// A boundary runs saturated: inference η-expands an under-applied call, so
/// a call that is not is one the checker never saw.
fn run_boundary(
    entry: &BuiltinEntry,
    argv: &[ValClos],
    site: Option<&Arc<Site>>,
    span: Option<Span>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    let arity = entry.decl.fixed_arity();
    if argv.len() != arity {
        let name = &entry.decl.name;
        return Err(invalid(
            name,
            InvalidCause::Arity(ArityFailure {
                expected: CountRule::Exactly(arity.into()),
                got: argv.len().into(),
            }),
        )
        .into());
    }
    super::call::run_native(entry, argv, site, span, mooring, shell)
}

pub(crate) fn close_argv(argv: &Argv, env: &Env, sig: &Signature) -> Result<Vec<ValClos>, Error> {
    let mut out = Vec::with_capacity(argv.len());
    for elem in argv {
        match elem {
            ValListElem::Single(v) => out.push(ValClos::close(&v.item, env, sig)?),
            ValListElem::Spread(v) => {
                let value = ValClos::close(&v.item, env, sig)?;
                match value.view() {
                    View::List(list) => out.extend(list.iter().map(Cow::into_owned)),
                    _ => return Err(spread_type_err(&value)),
                }
            }
        }
    }
    Ok(out)
}

pub(crate) fn close_redirects(
    redirects: &Redirects<Val>,
    env: &Env,
    sig: &Signature,
) -> Result<Redirects<String>, Error> {
    redirects.try_map(|v| ValClos::close(v, env, sig).map(|v| v.to_string()))
}

/// A directory or plain file ran as a command.
/// `e`, offering a path head given no arguments as text: it was most likely
/// meant as such.
fn offer_path_as_text(e: Error, head: &CommandWord, argv: &[ValClos]) -> Error {
    match head.name() {
        path @ (CommandName::Path(_) | CommandName::TildePath(_)) if argv.is_empty() => e
            .with_hint(format!(
                "to use the path as text, write `#'{}'#`",
                path.written()
            )),
        _ => e,
    }
}

fn render_handler_args(name: &str, arity: HandlerArity, argv: &[ValClos]) -> Vec<ValClos> {
    let list = ValClos::list(
        ValClos::render_argv(argv)
            .into_iter()
            .map(ValClos::string)
            .collect(),
    );
    match arity {
        HandlerArity::CatchAll => vec![ValClos::string(name), list],
        HandlerArity::Unary => vec![list],
    }
}

/// Room for frames, handed out only after the stack cap check.
struct Slot<'a, 'm> {
    machine: &'a mut Machine<'m>,
    count: usize,
}

impl Slot<'_, '_> {
    fn push(self, frame: Frame) {
        self.extend(std::iter::once(frame));
    }

    fn extend(self, frames: impl ExactSizeIterator<Item = Frame>) {
        assert_eq!(frames.len(), self.count, "push must match its reservation");
        for frame in frames {
            self.machine.push_frame(frame);
        }
    }
}

impl<'m> Machine<'m> {
    fn push_frame(&mut self, frame: Frame) {
        let call_site = usize::from(matches!(frame, Frame::CallSite(_)));
        self.stack.push(frame);
        self.call_site_frames += call_site;
    }

    fn pop_frame(&mut self) -> Option<Frame> {
        let frame = self.stack.pop()?;
        self.call_site_frames -= usize::from(matches!(frame, Frame::CallSite(_)));
        Some(frame)
    }

    /// The stack cap: refuse `n` more frames past `shell.session.stack_limit`,
    /// before any effect — every pushing rule calls this first.
    fn room(&self, n: usize, shell: &Shell) -> Settled<()> {
        let depth = self.stack.len() - self.call_site_frames;
        if depth + n > shell.session.stack_limit {
            return Err(
                Error::new(Reason::State(Box::new(StateFailure::Depth(DepthFailure {
                    kind: DepthKind::EvalStack,
                    limit: shell.session.stack_limit.into(),
                }))))
                .into(),
            );
        }
        Ok(())
    }

    fn reserve(&mut self, shell: &Shell) -> Settled<Slot<'_, 'm>> {
        self.reserve_n(1, shell)
    }

    fn reserve_n(&mut self, count: usize, shell: &Shell) -> Settled<Slot<'_, 'm>> {
        self.room(count, shell)?;
        Ok(Slot {
            machine: self,
            count,
        })
    }

    /// One `Arg` frame per argument, the first on top, where the head's
    /// first λ meets it.
    fn push_args(&mut self, args: Vec<ValClos>, span: Option<Span>, shell: &Shell) -> Settled<()> {
        self.reserve_n(args.len(), shell)?.extend(
            args.into_iter()
                .rev()
                .map(|value| Frame::Arg { value, span }),
        );
        Ok(())
    }

    fn step(&mut self, shell: &mut Shell) -> ControlFlow<Settled<ValClos>> {
        let next = match std::mem::replace(&mut self.state, VACANT) {
            Err(b) => match self.pop_frame() {
                None => return ControlFlow::Break(Err(b)),
                Some(frame) => self.unwind(frame, b, shell),
            },
            Ok((comp, env)) if matches!(comp.item, CompKind::Lam { .. }) => {
                match self.pop_frame() {
                    None => return ControlFlow::Break(Ok(boxed(Either::Left((comp, env))))),
                    Some(frame) => self.resume(frame, Either::Left((comp, env)), shell),
                }
            }
            Ok((comp, env)) => self.eval(&comp, env, shell),
        };
        self.settle(next, shell)
    }

    fn settle(
        &mut self,
        mut next: Settled<Next>,
        shell: &mut Shell,
    ) -> ControlFlow<Settled<ValClos>> {
        self.state = loop {
            let v = match next {
                Ok(Either::Right(v)) => v,
                Ok(Either::Left(closure)) => break Ok(closure),
                Err(b) => break Err(b),
            };
            next = match self.pop_frame() {
                None => return ControlFlow::Break(Ok(v)),
                Some(frame) => self.resume(frame, Either::Right(v), shell),
            };
        };
        ControlFlow::Continue(())
    }

    // ── call, force ────────────────────────────────────────────────────

    fn enter_call(&mut self, name: impl Into<FrameName>, span: Option<Span>, shell: &mut Shell) {
        if let Some(prev) = shell.enter_frame(name, span)
            && !matches!(self.stack.last(), Some(Frame::CallSite(_)))
        {
            self.push_frame(Frame::CallSite(Box::new(prev)));
        }
    }

    fn call(
        &mut self,
        f: ValClos,
        args: Vec<ValClos>,
        span: Option<Span>,
        shell: &mut Shell,
    ) -> Settled<Next> {
        if let View::Builtin(entry) = f.view()
            && args.len() == entry.decl.fixed_arity()
        {
            return Ok(Either::Right(self.run_native(
                &Arc::clone(entry),
                &args,
                span,
                shell,
            )?));
        }
        self.push_args(args, span, shell)?;
        Ok(Either::Left(force(f)?))
    }

    fn guard_release(
        &mut self,
        release: ValClos,
        resource: ValClos,
        outcome: Settled<ValClos>,
        span: Option<Span>,
        shell: &mut Shell,
    ) -> Settled<Next> {
        let mooring = self.mooring;
        let ending = exit_case(&outcome, shell);
        let slot = self.reserve(shell).expect("room reserved at guard entry");
        let prev = shell.shield(mooring.cleanup_grace);
        slot.push(Frame::Cleanup { outcome, prev });
        self.call(release, vec![resource, ending], span, shell)
            .map_err(|b| b.locate("guard", span, shell))
    }

    fn run_native(
        &self,
        entry: &BuiltinEntry,
        args: &[ValClos],
        span: Option<Span>,
        shell: &mut Shell,
    ) -> Settled<ValClos> {
        let name = FrameName::from(entry.decl.name.clone());
        shell.at_site(name.clone(), span, |shell| {
            super::call::run_native(entry, args, None, span, self.mooring, shell)
                .map_err(|b| b.locate(&name, span, shell))
        })
    }

    /// `push_redirect(redirs)`: nothing when empty, else install and push
    /// the `Redirect` frame.
    fn push_redirect(
        &mut self,
        redirs: &Redirects<String>,
        span: Option<Span>,
        shell: &mut Shell,
    ) -> Settled<()> {
        let mooring = self.mooring;
        if redirs.is_empty() {
            return Ok(());
        }
        let slot = self.reserve(shell)?;
        let state = RedirectState::install(redirs, span, mooring, shell)?;
        slot.push(Frame::Redirect(Box::new(state)));
        Ok(())
    }

    // ── Rules on a computation closure in focus ────────────────────────

    fn eval(&mut self, comp: &Arc<Comp>, env: Env, shell: &mut Shell) -> Settled<Next> {
        match operation_name(&comp.item) {
            Some(name) => shell.at_site(name.clone(), comp.span, |shell| {
                self.eval_rules(comp, env, shell)
                    .map_err(|b| b.locate(&name, comp.span, shell))
            }),
            None => self.eval_rules(comp, env, shell),
        }
    }

    /// Dispatch claims its own refusals; other operations use `eval`.
    fn eval_rules(&mut self, comp: &Arc<Comp>, env: Env, shell: &mut Shell) -> Settled<Next> {
        let mooring = self.mooring;
        Ok(match &comp.item {
            CompKind::Return(val) => Either::Right(ValClos::close(val, &env, &shell.sig)?),

            CompKind::Use(module) => {
                Either::Right(crate::shell::load::run_module(module, mooring, shell)?)
            }

            CompKind::Lam { .. } => unreachable!("`step` meets a λ against the frame below"),

            CompKind::Builtin { builtin, args } => {
                let value = env.get(builtin).expect("η binds its builtin");
                let View::Builtin(entry) = value.view() else {
                    unreachable!("η binds its builtin")
                };
                let args = args
                    .iter()
                    .map(|name| env.get(name).expect("η's λs bind its arguments").clone())
                    .collect::<Vec<_>>();
                Either::Right(self.run_native(entry, &args, comp.span, shell)?)
            }

            CompKind::Rec { group, index } => {
                shell.poll()?;
                // ρ|occ(g), computed once: the identity after the first unfold.
                let restricted = env.restrict(group.occ());
                let mut env2 = restricted.clone();
                env2.extend(group.shape().iter().enumerate().map(|(j, (name, _))| {
                    // The focused member's thunk is `comp` itself, no allocation.
                    let thunk_comp = if j == *index {
                        Arc::clone(comp)
                    } else {
                        Arc::new(Spanned::synthetic(CompKind::Rec {
                            group: Arc::clone(group),
                            index: j,
                        }))
                    };
                    (
                        name.clone(),
                        Binding {
                            value: ValClos::thunk(thunk_comp, &restricted),
                            scheme: None,
                        },
                    )
                }));
                Either::Left((Arc::clone(&group.shape()[*index].1), env2))
            }

            CompKind::Tilde(path) => {
                Either::Right(ValClos::string(path.shown(&shell.context.require_home()?)))
            }

            CompKind::Force(val) => {
                let name = forced_name(val).map_or(FrameName::Static("!"), FrameName::from);
                self.enter_call(name.clone(), comp.span, shell);
                force_val(val, env, &shell.sig)
                    .map(Either::Left)
                    .map_err(Break::from)
                    .map_err(|b| b.locate(&name, comp.span, shell))?
            }

            CompKind::Prim(prim) => Either::Right(expr::eval_prim(prim, env, shell)?),

            CompKind::Bind {
                comp: rhs,
                hole: (),
                pattern: _,
                rest: _,
            } => {
                shell.poll()?;
                self.reserve(shell)?.push(Frame::To {
                    bind: Arc::clone(comp),
                    env: env.clone(),
                });
                Either::Left((Arc::clone(rhs), env))
            }

            CompKind::Seq { first, rest } => {
                shell.poll()?;
                self.reserve(shell)?.push(Frame::Seq {
                    rest: Arc::clone(rest),
                    env: env.clone(),
                });
                Either::Left((Arc::clone(first), env))
            }

            CompKind::If { cond, then, else_ } => {
                let condition = ValClos::close(&cond.item, &env, &shell.sig)?;
                let View::Bool(b) = condition.view() else {
                    return Err(condition
                        .expected("if", Expected::Shape(ValueShape::Bool))
                        .into());
                };
                Either::Left(force_val(
                    &if b { then } else { else_ }.item,
                    env,
                    &shell.sig,
                )?)
            }

            CompKind::Case { scrutinee, arms } => {
                self.step_case(scrutinee, arms, comp.span, env, shell)?
            }

            CompKind::App {
                head,
                args,
                hole: (),
            } => {
                let name = match &head.item {
                    CompKind::Force(val) => forced_name(val),
                    _ => None,
                }
                .map_or(FrameName::Static("apply"), FrameName::from);
                self.enter_call(name.clone(), comp.span, shell);
                let call = (|| {
                    shell.poll()?;
                    let args = args
                        .iter()
                        .map(|arg| ValClos::close(&arg.item, &env, &shell.sig))
                        .collect::<Result<Vec<_>, _>>()?;
                    self.push_args(args, comp.span, shell)?;
                    Ok(Either::Left((Arc::clone(head), env)))
                })();
                call.map_err(|b: Break| b.locate(&name, comp.span, shell))?
            }

            CompKind::Exec(exec) => self.step_exec(exec, comp.span, &env, shell)?,

            &CompKind::BindingCall { hole, .. } => match hole {},

            CompKind::Pipeline { stages } => Either::Right(pipeline::PipeNode::run(
                stages, &env, STAGE_EVAL, mooring, shell,
            )?),

            CompKind::Capture(body) => {
                let slot = self.reserve(shell)?;
                let (sink, buf) = io::new_buffer();
                let prev = std::mem::replace(&mut shell.io.stdout, sink);
                slot.push(Frame::Capture {
                    prev,
                    buf,
                    span: comp.span,
                });
                Either::Left((Arc::clone(body), env))
            }

            CompKind::Redirect { body, redirects } => {
                let redirs = close_redirects(redirects, &env, &shell.sig)?;
                self.push_redirect(&redirs, comp.span, shell)?;
                Either::Left((Arc::clone(body), env))
            }

            CompKind::Within { opts, body } => {
                let opts = ValClos::close(&Val::Record(Arc::clone(opts)), &env, &shell.sig)?;
                let scope = WithinScope::parse(opts.as_map("within")?, shell)?;
                let body = ValClos::close(body, &env, &shell.sig)?;
                let slot = self.reserve(shell)?;
                slot.push(Frame::Within(scope.enter(shell)));
                Either::Left(force(body)?)
            }

            CompKind::Grant { caps, body } => {
                let c = ValClos::close(&Val::Record(Arc::clone(caps)), &env, &shell.sig)?;
                let caps = decode_capability_map(&c, "grant", &shell.freeze_ctx())?;
                let body = ValClos::close(body, &env, &shell.sig)?;
                let slot = self.reserve(shell)?;
                let at = shell.context.grants.len();
                shell.context.grants.push(caps);
                shell.audit_deputy_prefixes();
                slot.push(Frame::Grant(at));
                Either::Left(force(body)?)
            }

            CompKind::Try { body, handler } => {
                let body = ValClos::close(body, &env, &shell.sig)?;
                let handler = ValClos::close(handler, &env, &shell.sig)?;
                self.reserve(shell)?.push(Frame::Try {
                    handler,
                    span: comp.span,
                });
                Either::Left(force(body)?)
            }

            CompKind::Guard {
                acquire,
                body,
                release,
            } => {
                // Acquire and Guard pop back to this depth; Cleanup and two Args need three slots.
                self.room(3, shell)?;
                shell.poll()?;
                let acquire = ValClos::close(acquire, &env, &shell.sig)?;
                let body = ValClos::close(body, &env, &shell.sig)?;
                let release = ValClos::close(release, &env, &shell.sig)?;
                let slot = self.reserve(shell).expect("room reserved at guard entry");
                let prev = shell.shield(mooring.cleanup_grace);
                slot.push(Frame::Acquire {
                    body,
                    release,
                    prev,
                    span: comp.span,
                });
                Either::Left(force(acquire)?)
            }

            CompKind::Audit { body } => {
                let body = ValClos::close(body, &env, &shell.sig)?;
                let slot = self.reserve(shell)?;
                slot.push(Frame::Audit {
                    scope: shell.local.audit.open(CapturePolicy::Bytes),
                });
                Either::Left(force(body)?)
            }

            CompKind::Spawn { title, body } => {
                let title = title
                    .as_ref()
                    .map(|t| legible_title(&ValClos::close(&t.item, &env, &shell.sig)?, "spawn"))
                    .transpose()?;
                let body = ValClos::close(body, &env, &shell.sig)?;
                Either::Right(concurrency::spawn_form(title, &body, mooring, shell)?)
            }

            CompKind::Gather { body } => {
                let body = ValClos::close(body, &env, &shell.sig)?;
                let slot = self.reserve(shell)?;
                let owner = BroodGuard::new(BroodOwner::Gather);
                let prev = std::mem::replace(
                    &mut shell.local.cradle,
                    Cradle::owning(shell.scope.clone(), owner.brood()),
                );
                slot.push(Frame::Gather {
                    owner,
                    prev,
                    span: comp.span,
                });
                Either::Left(force(body)?)
            }
        })
    }

    /// `case`: the payload is pushed, and the chosen arm forced.
    fn step_case(
        &mut self,
        scrutinee: &Spanned<Val>,
        arms: &[CaseArm],
        span: Option<Span>,
        env: Env,
        shell: &Shell,
    ) -> Settled<Next> {
        let value = ValClos::close(&scrutinee.item, &env, &shell.sig)?;
        let View::Variant { label, payload } = value.view() else {
            return Err(value
                .expected("case", Expected::Shape(ValueShape::Variant))
                .into());
        };
        let Some(arm) = arms
            .iter()
            .find(|arm| arm.tag.item.as_str() == label.as_ref())
        else {
            let handled: Vec<String> = arms.iter().map(|a| a.tag.item.clone()).collect();
            return Err(Error::new(Reason::Evaluation(Box::new(EvalFailure::CaseArm(
                CaseArmFailure {
                    tag: label.to_string(),
                    handled,
                },
            ))))
            .into());
        };
        let payload = payload.map_or(ValClos::UNIT, Cow::into_owned);
        self.push_args(vec![payload], span, shell)?;
        Ok(Either::Left(force_val(&arm.body.item, env, &shell.sig)?))
    }

    fn step_exec(
        &mut self,
        exec: &crate::ir::Exec,
        span: Option<Span>,
        env: &Env,
        shell: &mut Shell,
    ) -> Settled<Next> {
        let name = FrameName::from(exec.head.name().written().into_owned());
        self.enter_call(name.clone(), span, shell);
        self.dispatch_exec(exec, span, env, shell)
            .map_err(|b| b.locate(&name, span, shell))
    }

    fn dispatch_exec(
        &mut self,
        exec: &crate::ir::Exec,
        span: Option<Span>,
        env: &Env,
        shell: &mut Shell,
    ) -> Settled<Next> {
        let mooring = self.mooring;
        shell.poll()?;
        let argv = close_argv(&exec.argv, env, &shell.sig)?;
        let redirs = close_redirects(&exec.redirects, env, &shell.sig)?;
        match Resolution::of(&exec.head, env, shell) {
            Resolution::Env(v) => {
                self.push_redirect(&redirs, span, shell)?;
                match v.view() {
                    View::Builtin(entry) if entry.decl.is_boundary() => Ok(Either::Right(
                        run_boundary(entry, &argv, exec.site.as_ref(), span, mooring, shell)?,
                    )),
                    _ => self.call(v, argv, span, shell),
                }
            }
            Resolution::Handler(HandlerLookup::Frame(entry, depth)) => {
                self.push_redirect(&redirs, span, shell)?;
                let slot = self.reserve(shell)?;
                let frame = Box::new(shell.context.handlers.strip_matched(depth));
                slot.push(Frame::Unmask { frame });
                let args = render_handler_args(&entry.name, entry.arity, &argv);
                self.call(entry.thunk.clone(), args, span, shell)
            }
            Resolution::Handler(HandlerLookup::Base(entry)) => {
                self.push_redirect(&redirs, span, shell)?;
                // Unlike a handler arm's, a native body's argv is not rendered.
                let v = entry.framed(span, shell, |shell, frame| {
                    entry.call_body(frame, &argv, None, mooring, shell)
                })?;
                Ok(Either::Right(v))
            }
            Resolution::External(command) => {
                let offer = |e| offer_path_as_text(e, &exec.head, &argv);
                let command = command.lookup(&shell.context.grants, shell.baseline());
                let names_no_program = matches!(
                    command.state(),
                    Err(LookupFailure::Missing(missing)) if missing.names_no_program()
                );
                let command = command
                    .admit(mooring, shell)
                    .map_err(|b| b.map_error(offer))?
                    .map_lookup_error(|e| if names_no_program { offer(e) } else { e });
                let v = command.run(&argv, &redirs, span, mooring, shell)?;
                Ok(Either::Right(v))
            }
        }
    }

    // ── Frames, and their two rules each ───────────────────────────────

    fn resume(&mut self, frame: Frame, t: Next, shell: &mut Shell) -> Settled<Next> {
        let mooring = self.mooring;
        match frame {
            Frame::CallSite(prev) => {
                shell.local.audit.restore_site(*prev);
                Ok(t)
            }
            Frame::To { bind, env } => {
                let v = boxed(t);
                let CompKind::Bind { pattern, rest, .. } = &bind.item else {
                    unreachable!("a To frame's `bind` is always a Bind comp")
                };
                let env = pattern::bind(pattern, &v, env)
                    .map_err(|b| b.locate("let", bind.span, shell))?;
                Ok(Either::Left((Arc::clone(rest), env)))
            }

            Frame::Seq { rest, env } => {
                drop(t);
                Ok(Either::Left((rest, env)))
            }

            // β: `⟨λx. M, ρ⟩` meeting `Arg(V)` is `⟨M, ρ[x ↦ V]⟩`.
            Frame::Arg { value, span } => match t {
                Either::Left((comp, mut env)) => {
                    let CompKind::Lam { param, body } = &comp.item else {
                        unreachable!("a λ is the only closure that meets a frame")
                    };
                    shell.poll().map_err(|b| b.locate("apply", span, shell))?;
                    env.bind(
                        param.clone(),
                        Binding {
                            value,
                            scheme: None,
                        },
                    );
                    Ok(Either::Left((located(body, span), env)))
                }
                Either::Right(v) => {
                    Err(Break::from(not_a_function(&v)).locate("apply", span, shell))
                }
            },

            Frame::Capture { prev, buf, span } => {
                shell.io.stdout = prev;
                let retained = buf.take();
                if retained.is_whole() {
                    Ok(Either::Right(ValClos::bytes(retained.bytes)))
                } else {
                    Err(Break::from(capture_overflowed()).locate("capture", span, shell))
                }
            }

            Frame::Redirect(state) => {
                state.commit(mooring, shell)?;
                Ok(t)
            }

            Frame::Try { .. } => Ok(t),

            Frame::Acquire {
                body,
                release,
                prev,
                span,
            } => {
                shell.scope = prev;
                let resource = boxed(t);
                self.reserve(shell)
                    .expect("room reserved at guard entry")
                    .push(Frame::Guard {
                        release,
                        resource: resource.clone(),
                        span,
                    });
                self.call(body, vec![resource], span, shell)
                    .map_err(|b| b.locate("guard", span, shell))
            }

            Frame::Guard {
                release,
                resource,
                span,
            } => {
                let outcome = Ok(boxed(t));
                self.guard_release(release, resource, outcome, span, shell)
            }

            Frame::Cleanup { outcome, prev } => {
                shell.scope = prev;
                outcome.map(Either::Right)
            }

            undo @ (Frame::Unmask { .. } | Frame::Within(_) | Frame::Grant(_)) => {
                undo.abandon(shell);
                Ok(t)
            }

            Frame::Audit { scope } => {
                let trail = shell.local.audit.close(scope);
                let v = boxed(t);
                Ok(Either::Right(report_value(Ok(v), trail)))
            }

            Frame::Gather { owner, prev, span } => {
                shell.local.cradle = prev;
                let v = concurrency::exit_brood(&owner, Ok(boxed(t)), mooring.cleanup_grace, shell)
                    .map_err(|b| b.locate("gather", span, shell))?;
                Ok(Either::Right(v))
            }
        }
    }

    fn unwind(&mut self, frame: Frame, s: Break, shell: &mut Shell) -> Settled<Next> {
        let mooring = self.mooring;
        match frame {
            Frame::CallSite(prev) => {
                let at = shell.local.audit.call_site();
                let s = s.locate(at.name(), at.span(), shell);
                shell.local.audit.restore_site(*prev);
                Err(s)
            }
            Frame::To { bind, .. } => {
                let CompKind::Bind { pattern, .. } = &bind.item else {
                    unreachable!("a To frame's `bind` is always a Bind comp")
                };
                Err(match s {
                    Break::Error(e) if !matches!(pattern.as_ref(), Pattern::Name(name) if is_synthetic(name)) => {
                        e.abandoning_tail().into()
                    }
                    s => s,
                })
            }

            Frame::Seq { .. } => Err(match s {
                Break::Error(e) => e.abandoning_tail().into(),
                s @ Break::Escape(_) => s,
            }),

            Frame::Capture { prev, buf, .. } => {
                shell.io.stdout = prev;
                let total = buf.take().total;
                Err(match s {
                    Break::Error(e) if total > 0 => {
                        e.discarded(u32::try_from(total).unwrap_or(u32::MAX)).into()
                    }
                    s => s,
                })
            }

            Frame::Arg { .. } => Err(s),

            Frame::Acquire { prev, .. } | Frame::Cleanup { prev, .. } => {
                shell.scope = prev;
                Err(s)
            }

            Frame::Redirect(state) => {
                state.abort(mooring, shell);
                Err(s)
            }

            undo @ (Frame::Unmask { .. } | Frame::Within(_) | Frame::Grant(_)) => {
                undo.abandon(shell);
                Err(s)
            }

            Frame::Try { handler, span } => match s {
                Break::Error(e) => {
                    let record = ValClos::failure(e.fact(shell));
                    self.call(handler, vec![record], span, shell)
                        .map_err(|b| b.locate("try", span, shell))
                }
                escape @ Break::Escape(_) => Err(escape),
            },

            Frame::Guard {
                release,
                resource,
                span,
            } => self.guard_release(release, resource, Err(s), span, shell),

            Frame::Audit { scope, .. } => match s {
                Break::Error(e) => {
                    let trail = shell.local.audit.close(scope);
                    let record = e.fact(shell);
                    Ok(Either::Right(report_value(Err(record), trail)))
                }
                escape @ Break::Escape(_) => {
                    drop(shell.local.audit.close(scope));
                    Err(escape)
                }
            },

            Frame::Gather { owner, prev, .. } => {
                shell.local.cradle = prev;
                owner.end(mooring.cleanup_grace);
                Err(s)
            }
        }
    }
}

impl Frame {
    /// Undo what a checkpoint cannot hold — fds, staging files, audit scopes —
    /// where the frame's effect is only undone: a panic's unwinding, or a rule
    /// that has nothing to read back.  `Capture` restores `io.stdout`;
    /// `Redirect` as its own rule; `Unmask` restores; `Audit` closes its scope,
    /// discarding the trail no one is left to read; `Within` applies its undo;
    /// `Grant` removes its layer; `Acquire` and `Cleanup` restore the scope;
    /// `Gather` restores the cradle and drops its owner to stop its brood
    /// without waiting. The rest do nothing.
    fn abandon(self, shell: &mut Shell) {
        match self {
            Self::CallSite(prev) => shell.local.audit.restore_site(*prev),
            Self::Capture { prev, .. } => shell.io.stdout = prev,
            Self::Redirect(state) => state.abandon(shell),
            Self::Unmask { frame } => shell.context.handlers.restore_matched(*frame),
            Self::Audit { scope, .. } => drop(shell.local.audit.close(scope)),
            Self::Within(undo) => undo.apply(shell),
            Self::Grant(at) => shell.context.grants.remove(at, 1),
            Self::Acquire { prev, .. } | Self::Cleanup { prev, .. } => shell.scope = prev,
            Self::Gather { prev, .. } => shell.local.cradle = prev,
            Self::To { .. }
            | Self::Seq { .. }
            | Self::Arg { .. }
            | Self::Try { .. }
            | Self::Guard { .. } => {}
        }
    }
}

// ── Boundaries ───────────────────────────────────────────────────────────

/// The nested-machine depth guard around `seed`, then `step` until the stack
/// is empty: `Ok` on a value, `Err` on a halt. `catch_unwind` around the
/// loop; on panic, `abandon` walks every frame top-down before resuming.
fn run(
    seed: impl FnOnce(&mut Machine<'_>, &mut Shell) -> Settled<Next>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    if shell.local.machine_depth >= NESTED_MACHINE_LIMIT {
        return Err(
            Error::new(Reason::State(Box::new(StateFailure::Depth(DepthFailure {
                kind: DepthKind::NativeReentry,
                limit: NESTED_MACHINE_LIMIT.into(),
            }))))
            .into(),
        );
    }
    shell.local.machine_depth += 1;
    let mut m = Machine::new(mooring);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let next = seed(&mut m, shell);
        let mut flow = m.settle(next, shell);
        loop {
            match flow {
                ControlFlow::Break(answer) => break answer,
                ControlFlow::Continue(()) => flow = m.step(shell),
            }
        }
    }));
    shell.local.machine_depth -= 1;
    match result {
        Ok(answer) => answer,
        Err(payload) => {
            while let Some(frame) = m.pop_frame() {
                frame.abandon(shell);
            }
            std::panic::resume_unwind(payload);
        }
    }
}

/// What the runtime's pipeline stages call back into: the one recursion.
pub(crate) const STAGE_EVAL: pipeline::StageEval = pipeline::StageEval {
    run: evaluate,
    close_argv,
};

/// The initial state ⟨M, E⟩ over the empty stack, then step until the stack
/// is empty.
pub(crate) fn evaluate(
    comp: Arc<Comp>,
    env: Env,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    run(|_, _| Ok(Either::Left((comp, env))), mooring, shell)
}

/// Enter a closed value with arguments; a value meeting none is forced.
pub(crate) fn apply(
    f: ValClos,
    args: Vec<ValClos>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    run(|m, shell| m.call(f, args, None, shell), mooring, shell)
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::io::Retained;
    use crate::ir::{GroupNode, Name, Occ, Phrase};
    use crate::shell::evaluator::{run_source, with_capture};
    use crate::shell::runtime::{Break, Escape, Stop, captured};

    /// Run every phrase of `source` through the machine: a `Define` binds
    /// into the running `Env` with `pattern::bind`, a `Run` phrase's
    /// value is the previous one, mirroring `run_phrases`'s shape without
    /// its lease machinery, irrelevant to what these tests probe.
    fn run_with(source: &str, mooring: &Mooring, shell: &mut Shell) -> Settled<ValClos> {
        let file =
            shell.install_script_context(&crate::source::SourceOrigin::named("<machine>"), source);
        let top = crate::compile::compile_and_typecheck(
            source,
            crate::test_helper::core_schemes(),
            file,
            None,
            crate::typecheck::Unit::Script,
            &mut crate::compile::no_modules,
        )
        .expect("compile");
        let mut env = shell.env.clone();
        let mut value = Ok(ValClos::UNIT);
        for phrase in &top.phrases {
            match &phrase.item {
                Phrase::Run(comp) => {
                    value = evaluate(comp.clone(), env.clone(), mooring, shell);
                    value.as_ref().map_err(Clone::clone)?;
                }
                Phrase::Define { pattern, comp, .. } => {
                    let v = evaluate(comp.clone(), env.clone(), mooring, shell)?;
                    env = pattern::bind(pattern, &v, env)?;
                    value = Ok(ValClos::UNIT);
                }
            }
        }
        shell.env = env;
        value
    }

    fn new_shell() -> Shell {
        crate::test_helper::core_shell()
    }

    /// The hints `source` fails with, when it fails.
    #[cfg(unix)]
    fn failure_hints(source: &str, shell: &mut Shell) -> Vec<String> {
        match run_source(source, shell) {
            Err(Break::Error(e)) => {
                let rendered = e.render(shell.sources(), None, shell.exit_hints());
                let compact = e.compact(shell.exit_hints());
                let hints: Vec<_> = crate::io::ansi::strip(&compact)
                    .lines()
                    .filter_map(|line| line.trim().strip_prefix("hint: ").map(str::to_owned))
                    .collect();
                let mut previous = 0;
                for hint in &hints {
                    assert_eq!(rendered.matches(hint.as_str()).count(), 1, "{rendered}");
                    let at = rendered.find(hint).expect("full report retains the hint");
                    assert!(at >= previous, "{rendered}");
                    previous = at + hint.len();
                }
                hints
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    /// The first hint `source` fails with, when it fails.
    #[cfg(unix)]
    fn failure_hint(source: &str, shell: &mut Shell) -> Option<String> {
        failure_hints(source, shell).into_iter().next()
    }

    #[cfg(unix)]
    const AS_TEXT: &str = "to use the path as text, write";

    /// A path head given no arguments offers itself as text, whether a grant
    /// refuses it or it names a directory or plain file; given arguments, it
    /// is a program call and offers nothing.
    #[cfg(unix)]
    #[test]
    fn an_argless_path_head_that_is_no_program_offers_itself_as_text() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::path::shown(tmp.path());
        let file = tmp.path().join("plain");
        std::fs::write(&file, "").unwrap();
        let file = crate::path::shown(&file);
        let mut shell = new_shell();
        for path in [&dir, &file] {
            let hint = failure_hint(path, &mut shell).expect("a hint");
            assert!(hint.contains(&format!("{AS_TEXT} `#'{path}'#`")), "{hint}");
            let hint = failure_hint(&format!("{path} x"), &mut shell);
            assert!(!hint.is_some_and(|h| h.contains(AS_TEXT)));
        }
    }

    /// The grant's refusal keeps its own hint, and the offer follows it.
    #[cfg(unix)]
    #[test]
    fn a_grant_refused_path_head_offers_itself_as_text_after_the_grants_hint() {
        use crate::authority::policy::{Capabilities, ExecGrant};
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::path::shown(tmp.path());
        let mut shell = new_shell();
        shell
            .context
            .grants
            .push(crate::test_helper::layer(Capabilities {
                exec: Some(ExecGrant::default()),
                ..Capabilities::default()
            }));
        let hints = failure_hints(&dir, &mut shell);
        let [grant, text] = hints.as_slice() else {
            panic!("expected the grant's hint then the offer: {hints:?}")
        };
        assert!(
            text.contains(&format!("{AS_TEXT} `#'{dir}'#`")),
            "{hints:?}"
        );
        assert!(
            grant.contains("grant") && !grant.contains("#'"),
            "{hints:?}"
        );
        assert_eq!(hints.iter().filter(|hint| *hint == grant).count(), 1);
        assert_eq!(hints.iter().filter(|hint| *hint == text).count(), 1);
        let hints = failure_hints(&format!("{dir} x"), &mut shell);
        assert!(
            !hints.is_empty() && hints.iter().all(|h| !h.contains(AS_TEXT)),
            "{hints:?}"
        );
    }

    /// `M to x. N`: a `let` in `rest` is invisible once the `To` frame pops
    /// — the extent is structural, not the session's persistent scope.
    #[test]
    fn to_extent_is_the_rest_of_its_block() {
        let mut shell = new_shell();
        let out = run_source(
            "let inner = 0\nif true { let inner = 1; return $inner }\nreturn $inner",
            &mut shell,
        );
        assert!(
            matches!(out.as_ref().map(ValClos::view), Ok(View::Int(0))),
            "the block's `inner` must not outlive it, got {out:?}"
        );
    }

    /// A `let` captures the whole command that produces its value.
    #[test]
    fn a_let_captures_the_command_that_produces_its_value() {
        let mut shell = new_shell();
        let (result, Retained { bytes, .. }) = with_capture(&mut shell, |shell| {
            run_source("let x = !{ echo a; echo b }\nreturn $x", shell)
        });
        assert!(
            matches!(result.as_ref().map(ValClos::view), Ok(View::String(s)) if s.as_str() == "a\nb")
        );
        assert!(bytes.is_empty(), "nothing escaped the capture");
    }

    /// A stand-in writes as the command it stands in for does, all of it, so a
    /// capture of the command takes every statement of the stand-in.
    #[test]
    fn a_captured_stand_ins_every_write_is_captured() {
        let mut shell = new_shell();
        let (result, Retained { bytes, .. }) = with_capture(&mut shell, |shell| {
            run_source(
                "within [handlers: [:, curl: { |a| echo note; echo body }]] {\n\
                 let x = curl\nreturn $x\n}",
                shell,
            )
        });
        assert!(
            matches!(result.as_ref().map(ValClos::view), Ok(View::String(s)) if s.as_str() == "note\nbody")
        );
        assert!(bytes.is_empty(), "nothing escaped the capture");
    }

    /// A capture that does not become its value commits nothing: what it
    /// wrote before failing is dropped, not written to the sink it replaced.
    #[test]
    fn a_failing_capture_writes_nothing_to_the_sink_it_replaced() {
        let mut shell = new_shell();
        let (result, Retained { bytes, .. }) = with_capture(&mut shell, |shell| {
            run_source(
                "within [handlers: [:, boom: { |a| echo partial; fail 'x' }]] {\n\
                 let x = boom\nreturn $x\n}",
                shell,
            )
        });
        assert!(result.is_err(), "the stand-in fails");
        assert!(bytes.is_empty(), "the failed capture wrote {bytes:?}");
    }

    /// A `try` around a failing captured pipeline binds the handler's value,
    /// and what the pipeline wrote never reaches the stream.
    #[test]
    fn a_try_around_a_failing_capture_yields_only_the_handlers_value() {
        let mut shell = new_shell();
        let (result, Retained { bytes, .. }) = with_capture(&mut shell, |shell| {
            run_source(
                "within [handlers: [:, boom: { |a| echo partial; fail 'x' }]] {\n\
                 let x = try { return !{ echo in | boom } } { |_| return 'handled' }\n\
                 return $x\n}",
                shell,
            )
        });
        assert!(
            matches!(result.as_ref().map(ValClos::view), Ok(View::String(s)) if s.as_str() == "handled")
        );
        assert!(bytes.is_empty(), "the failed capture wrote {bytes:?}");
    }

    #[test]
    fn nested_failing_captures_sum_their_bytes_into_one_note() {
        let mut shell = new_shell();
        let (result, Retained { bytes, .. }) = with_capture(&mut shell, |shell| {
            run_source(
                "within [handlers: [:, boom: { |a| echo cde; fail 'x' }]] {\n\
                 let x = !{ echo ab; let y = boom; echo z }\nreturn $x\n}",
                shell,
            )
        });
        assert!(bytes.is_empty(), "the failed captures wrote {bytes:?}");
        let Err(Break::Error(e)) = result else {
            panic!("expected the inner failure, got {result:?}")
        };
        assert_eq!(e.unwound().discarded, 7, "ab\\n plus cde\\n");
        assert_eq!(e.hints(), []);
        let rendered = e.render(shell.sources(), None, shell.exit_hints());
        assert_eq!(rendered.matches("were discarded").count(), 1, "{rendered}");
        assert!(rendered.contains("7 bytes written to stdout"), "{rendered}");
    }

    #[test]
    fn discard_and_abandoned_tail_notes_render_in_that_order() {
        let mut shell = new_shell();
        let (result, Retained { bytes, .. }) = with_capture(&mut shell, |shell| {
            run_source(
                "let x = !{ echo ab; fail 'x'; echo later }; return $x",
                shell,
            )
        });
        assert_eq!(bytes, [] as [u8; 0]);
        let Err(Break::Error(e)) = result else {
            panic!("expected failure")
        };
        assert_eq!(e.unwound().discarded, 3);
        assert!(e.unwound().abandoned_tail);
        assert_eq!(e.hints(), []);
        let rendered = e.render(shell.sources(), None, shell.exit_hints());
        let discard = rendered
            .find("3 bytes written to stdout")
            .expect("discard note");
        let tail = rendered
            .find("later steps in this block did not run")
            .expect("tail note");
        assert!(discard < tail, "{rendered}");
    }

    /// An escape out of a capture drops what it wrote, like a failure.
    #[test]
    fn an_exit_inside_a_capture_writes_nothing() {
        let mut shell = new_shell();
        let (result, Retained { bytes, .. }) = with_capture(&mut shell, |shell| {
            run_source(
                "within [handlers: [:, boom: { |a| echo partial; exit 3 }]] {\n\
                 let x = boom\nreturn $x\n}",
                shell,
            )
        });
        assert!(
            matches!(result, Err(Break::Escape(Escape::Exit(3)))),
            "expected the exit to propagate, got {result:?}"
        );
        assert!(bytes.is_empty(), "the abandoned capture wrote {bytes:?}");
    }

    /// `a ? b ? c`: an `Error` arm falls through to the next; an `Escape`
    /// propagates without trying a fallback.
    #[test]
    fn chain_falls_through_on_error_and_propagates_escape() {
        let mut shell = new_shell();
        run_source("false ? true", &mut shell).expect("second arm must run");

        let mut shell = new_shell();
        let out = run_source("exit 5 ? true", &mut shell);
        match out {
            Err(Break::Escape(crate::shell::runtime::Escape::Exit(5))) => {}
            other => panic!("expected Escape::Exit(5) to propagate past the chain, got {other:?}"),
        }
    }

    /// A value meeting an argument is unreachable for checked code.
    #[test]
    fn a_value_meeting_an_argument_is_located_at_the_call() {
        let mut shell = new_shell();
        let file = shell.install_script_context(
            &crate::source::SourceOrigin::named("<call>"),
            "                     ",
        );
        let span = crate::source::Span::new(file, 10, 20);
        let head = Arc::new(Spanned::with_span(
            Some(span),
            CompKind::Return(Val::Int(5)),
        ));
        let args = Box::new([Spanned::with_span(Some(span), Val::Int(1))]);
        let app = Spanned::with_span(
            Some(span),
            CompKind::App {
                head,
                args,
                hole: (),
            },
        );
        let out = evaluate(
            Arc::new(app),
            shell.env.clone(),
            &Mooring::adrift(),
            &mut shell,
        );
        match out {
            Err(Break::Error(e)) => {
                assert!(
                    matches!(&e.reason, Reason::Invariant(f) if **f == InvariantFailure::AppliedUnforcedValue(ValueShape::Int)),
                    "got {:?}",
                    e.describe()
                );
                assert_eq!(
                    e.prov.as_ref().and_then(crate::shell::runtime::Prov::span),
                    Some(span),
                    "the value application error must carry the call's span"
                );
            }
            other => panic!("expected a not-a-function error, got {other:?}"),
        }
    }

    #[test]
    fn function_typed_right_hand_sides_run_once_in_order() {
        for (source, expected) in [
            (
                "let add = { |a b| return $[$a + $b] }; \
                 let g = if true { echo effect-ran; add 1 } else { add 2 }; \
                 echo 'bound g'; echo !{g 3}",
                "effect-ran\nbound g\n4\n",
            ),
            (
                "let add = { |a b| return $[$a + $b] }; \
                 let g = try { echo body-ran; fail boom; add 1 } { |e| add 2 }; \
                 echo 'bound g'; echo !{g 3}",
                "body-ran\nbound g\n5\n",
            ),
            (
                "let add = { |a b| return $[$a + $b] }; \
                 let h = { |x| echo once; add $x }; let g = h 1; \
                 echo !{g 2}; echo !{g 3}",
                "once\n3\n4\n",
            ),
        ] {
            let mut shell = new_shell();
            let (result, retained) = with_capture(&mut shell, |shell| run_source(source, shell));
            result.expect("function-typed right-hand side must run");
            assert!(retained.is_whole());
            assert_eq!(retained.bytes, expected.as_bytes(), "{source}");
        }
    }

    #[test]
    fn apply_returns_a_residual_closure_over_the_supplied_argument() {
        let mut shell = new_shell();
        let f = run_source("{ |a b| return $[$a + $b] }", &mut shell).expect("closure");
        let mooring = Mooring::adrift();
        let partial = apply(f, vec![ValClos::int(1)], &mooring, &mut shell).expect("partial");
        let View::Thunk { comp, env } = partial.view() else {
            panic!("expected a residual closure, got {partial:?}")
        };
        assert_eq!(comp.lam().map(|(param, _)| param.as_ref()), Some("b"));
        assert_eq!(env.get("a"), Some(&ValClos::int(1)));
        let out = apply(partial, vec![ValClos::int(2)], &mooring, &mut shell).expect("sum");
        assert_eq!(out, ValClos::int(3));
    }

    #[test]
    fn a_partial_map_runs_when_saturated() {
        let mut shell = new_shell();
        let out = run_source(
            "let inc = { |x| return $[$x + 1] }; let m = map $inc; m [1, 2]",
            &mut shell,
        )
        .expect("saturated map");
        assert_eq!(out, ValClos::list(vec![ValClos::int(2), ValClos::int(3)]));
    }

    #[test]
    fn forcing_an_arity_zero_native_runs_its_body() {
        let mut shell = new_shell();
        let entry = shell.lookup_builtin("reset").expect("reset builtin");
        let reset = ValClos::builtin(entry.into());
        let (out, retained) = with_capture(&mut shell, |shell| {
            apply(reset, vec![], &Mooring::adrift(), shell)
        });
        assert_eq!(out.expect("forced reset"), ValClos::UNIT);
        assert!(retained.is_whole());
        assert_eq!(retained.bytes, b"\x1bc");
    }

    #[test]
    fn apply_returns_the_eta_residual_over_its_supplied_arguments() {
        let mut shell = new_shell();
        let entry = shell.lookup_builtin("has").expect("has builtin");
        let has = ValClos::builtin(entry.into());
        assert_eq!(has.to_string(), "<native has>");
        let mooring = Mooring::adrift();
        let record = ValClos::map(vec![("present".into(), ValClos::int(7))]);
        let partial = apply(has, vec![record], &mooring, &mut shell).expect("partial has");
        assert_eq!(partial.to_string(), "<native has, 1 of 2 arguments>");
        assert_eq!(partial.type_name(), "Lambda");
        let out = apply(
            partial,
            vec![ValClos::string("present")],
            &mooring,
            &mut shell,
        )
        .expect("saturated has");
        assert_eq!(out, ValClos::bool(true));
    }

    #[test]
    fn an_under_applied_builtin_in_a_body_takes_the_callers_next_argument() {
        let mut shell = new_shell();
        let out = run_source(
            "let m = { |f| map $f }; m { |x| return $[$x + 1] } [1, 2]",
            &mut shell,
        )
        .expect("the residual lambda crosses the body's call site");
        assert_eq!(out, ValClos::list(vec![ValClos::int(2), ValClos::int(3)]));
    }

    #[test]
    fn native_arg_errors_name_the_builtin_at_the_saturating_call() {
        let mut shell = new_shell();
        let f = run_source("{ |x| return $x }", &mut shell).expect("callback");
        let entry = shell.lookup_builtin("map").expect("map builtin");
        let map = ValClos::builtin(entry.into());
        let mooring = Mooring::adrift();
        let partial = apply(map, vec![f], &mooring, &mut shell).expect("partial map");
        crate::test_access::bind_untyped(&mut shell, "held", partial);
        let file = shell.install_script_context(
            &crate::source::SourceOrigin::named("<native-call>"),
            "held           held 1",
        );
        let force_span = Span::new(file, 0, 4);
        let call_span = Span::new(file, 15, 21);
        let app = Arc::new(Spanned::with_span(
            Some(call_span),
            CompKind::App {
                head: Arc::new(Spanned::with_span(
                    Some(force_span),
                    CompKind::Force(Val::Variable("held".into())),
                )),
                args: Box::new([Spanned::with_span(Some(call_span), Val::Int(1))]),
                hole: (),
            },
        ));
        let out = evaluate(app, shell.env.clone(), &mooring, &mut shell);
        let Err(Break::Error(error)) = out else {
            panic!("expected map's list refusal, got {out:?}")
        };
        let prov = error.prov.as_ref().expect("located native refusal");
        assert_eq!(prov.name(), "map");
        assert_eq!(prov.span(), Some(call_span));
    }

    #[test]
    fn a_boxed_boundary_lambda_captures_only_what_it_mentions() {
        let mut shell = new_shell();
        let source = format!(
            "let big = '{}'; let decode = from-json-at; return $decode",
            "payload".repeat(4096),
        );
        let out = run_source(&source, &mut shell).expect("partial boundary");
        let View::Thunk { comp, env } = out.view() else {
            panic!("expected a boxed boundary lambda, got {out:?}")
        };
        assert!(matches!(comp.item, CompKind::Lam { .. }));
        assert!(env.binding("big").is_none());
        let occ = Occ::of(comp);
        assert!(env.iter().all(|(name, _)| occ.contains(name)));
    }

    /// `apply` — the boundary natives reach `machine::apply` through —
    /// meeting a closed lambda value directly, with no `Comp` in sight.
    #[test]
    fn apply_meets_a_closed_lambda_value() {
        let mut shell = new_shell();
        let f = run_source("{ |a b| return $[$a + $b] }", &mut shell).expect("closure must close");
        let mooring = Mooring::adrift();
        let out = apply(
            f,
            vec![ValClos::int(1), ValClos::int(2)],
            &mooring,
            &mut shell,
        )
        .expect("apply must succeed");
        assert_eq!(out, ValClos::int(3));
    }

    /// The `Rec` rule reuses ρ|occ(g): forcing a sibling from one unfold and
    /// unfolding again from its own environment yields new siblings whose
    /// environment is the same root, `ptr_eq`, as the one that forced them —
    /// `restrict` recognises it need not rebuild. A group of one binds its
    /// own member to the node already in focus, no allocation.
    #[test]
    fn a_recursive_call_reuses_its_node_and_its_environment() {
        let mut shell = new_shell();
        let mooring = Mooring::adrift();
        crate::test_access::bind_untyped(&mut shell, "k", ValClos::int(1));

        // `even`/`odd`, both closing over the session `k`: occ(g) = {k}.
        let even_body = Arc::new(Spanned::synthetic(CompKind::Return(Val::Variable(
            "k".into(),
        ))));
        let odd_body = Arc::new(Spanned::synthetic(CompKind::Return(Val::Variable(
            "k".into(),
        ))));
        let group = GroupNode::new(Box::from([
            (Name::from("even"), even_body),
            (Name::from("odd"), odd_body),
        ]));
        let rec_even = Arc::new(Spanned::synthetic(CompKind::Rec {
            group: Arc::clone(&group),
            index: 0,
        }));

        let mut machine = Machine::new(&mooring);
        assert!(
            machine
                .settle(Ok(Either::Left((rec_even, shell.env.clone()))), &mut shell)
                .is_continue()
        );
        assert!(machine.step(&mut shell).is_continue());
        let (_, env1) = machine.state.as_ref().expect("first unfold");
        let odd1 = env1.get("odd").cloned().expect("odd bound");
        let (odd_comp, forced_env) = force(odd1).expect("odd must be a thunk");
        assert!(
            machine
                .settle(Ok(Either::Left((odd_comp, forced_env.clone()))), &mut shell)
                .is_continue()
        );
        assert!(machine.step(&mut shell).is_continue());
        let (_, env2) = machine.state.as_ref().expect("second unfold");
        let even2 = env2.get("even").expect("even bound");
        assert!(
            even2.env().expect("even is a thunk").ptr_eq(&forced_env),
            "a later unfold's siblings hold the environment that forced it, unchanged"
        );

        // A group of one: its own member is the node already in focus.
        let self_body = Arc::new(Spanned::synthetic(CompKind::Return(Val::Unit)));
        let solo = GroupNode::new(Box::from([(Name::from("f"), self_body)]));
        let rec_solo = Arc::new(Spanned::synthetic(CompKind::Rec {
            group: Arc::clone(&solo),
            index: 0,
        }));
        assert!(
            machine
                .settle(
                    Ok(Either::Left((Arc::clone(&rec_solo), shell.env.clone()))),
                    &mut shell
                )
                .is_continue()
        );
        assert!(machine.step(&mut shell).is_continue());
        let (_, solo_env) = machine.state.as_ref().expect("solo unfold");
        let solo_closure = solo_env.get("f").expect("f bound");
        let View::Thunk { comp, .. } = solo_closure.view() else {
            panic!("f must be a thunk");
        };
        assert!(
            Arc::ptr_eq(comp, &rec_solo),
            "a group of one's own member is the node already in focus"
        );
    }

    /// A literal forms without building: it equals the built value
    /// element-wise, and a record iterates in a built map's order. A literal
    /// naming nothing captures the static empty environment.
    #[test]
    fn a_literal_forms_without_building() {
        let mut shell = new_shell();
        let out = run_source(
            "let x = 1\nlet r = [b: [$x, 2], a: $x]\nreturn $r",
            &mut shell,
        )
        .expect("a literal record must form");
        let View::Map(record) = out.view() else {
            panic!("expected a record, got {out:?}")
        };
        assert!(
            out.env().is_some(),
            "a literal forms as a value closure, not built data"
        );

        let expected = ValClos::map(vec![
            (
                "b".to_string(),
                ValClos::list(vec![ValClos::int(1), ValClos::int(2)]),
            ),
            ("a".to_string(), ValClos::int(1)),
        ]);
        assert_eq!(out, expected, "a literal reads the same as the built value");
        assert_eq!(
            record
                .iter()
                .map(|(k, _)| k.to_string())
                .collect::<Vec<_>>(),
            ["a", "b"],
            "a literal record iterates in a built map's order"
        );

        let inner = record.get("b").expect("a list field").into_owned();
        assert!(matches!(inner.view(), View::List(_)), "expected a list");
        assert!(
            inner.env().is_some(),
            "a nested list literal also forms as a value closure"
        );

        let out = run_source("let l = [1, 2]\nreturn $l", &mut shell)
            .expect("a literal naming nothing must still form");
        assert!(
            matches!(out.view(), View::List(_)),
            "expected a list, got {out:?}"
        );
        let env = out
            .env()
            .expect("a literal naming nothing is still a literal");
        assert!(
            env.ptr_eq(&Env::new()),
            "a literal naming nothing captures the static empty environment"
        );
    }

    /// Two runs, as two turns: within one, the grouping pre-pass would lift
    /// the later `let` above `h`.
    #[test]
    fn a_definition_made_after_a_closure_is_invisible_to_it() {
        let mut shell = new_shell();
        run_source("let h = { zqx-cmd }", &mut shell).expect("define h");
        let out = run_source("let zqx-cmd = { 1 }\n!$h", &mut shell);
        assert!(
            out.is_err(),
            "`h` must not see a later `zqx-cmd`, got {out:?}"
        );
        assert!(captured(shell.env.get("h")).binding("zqx-cmd").is_none());
    }

    #[test]
    fn a_block_captures_only_the_names_it_mentions() {
        let mut shell = new_shell();
        run_source(
            "let big = 'payload'\nlet f = { 1 }\nlet g = { $big }\nlet h = { echo $big }",
            &mut shell,
        )
        .expect("define");
        let holds_big = |name| captured(shell.env.get(name)).binding("big").is_some();
        assert!(!holds_big("f"), "`f` does not mention `big`");
        assert!(
            holds_big("g") && holds_big("h"),
            "`g` and `h` mention `big`"
        );
    }

    #[test]
    fn a_recursive_group_captures_what_it_mentions() {
        let mut shell = new_shell();
        let out = run_source(
            "let k = 1\n\
             let big = 'payload'\n\
             let even = { |n| if $[$n == 0] { return true } else { odd $[$n - 1] } }\n\
             let odd = { |n| if $[$n == 0] { return false } else { even $[$n - $k] } }\n\
             even 10",
            &mut shell,
        )
        .expect("mutual recursion must terminate");
        assert!(
            matches!(out.view(), View::Bool(true)),
            "expected true, got {out:?}"
        );
        for name in ["even", "odd"] {
            let env = captured(shell.env.get(name));
            assert!(env.binding("k").is_some(), "`{name}` holds `k`");
            assert!(env.binding("big").is_none(), "`{name}` lacks `big`");
        }
    }

    #[test]
    fn call_site_accounting_tracks_coalescing_return_and_halt() {
        let mut shell = new_shell();
        shell.session.stack_limit = 2;
        let file = shell
            .install_script_context(&crate::source::SourceOrigin::named("<accounting>"), "f g");
        let span = Some(Span::new(file, 0, 1));
        let before = shell.local.audit.call_site().clone();
        let mooring = Mooring::adrift();
        let mut machine = Machine::new(&mooring);
        machine.enter_call("f", span, &mut shell);
        machine.enter_call("g", span, &mut shell);
        assert_eq!((machine.stack.len(), machine.call_site_frames), (1, 1));
        machine
            .push_args(vec![ValClos::UNIT, ValClos::UNIT], span, &shell)
            .expect("provenance costs no computational frames");
        machine.enter_call("f", span, &mut shell);
        machine.enter_call("g", span, &mut shell);
        assert_eq!((machine.stack.len(), machine.call_site_frames), (4, 2));
        assert!(machine.reserve(&shell).is_err());
        assert_eq!((machine.stack.len(), machine.call_site_frames), (4, 2));

        assert!(
            machine
                .settle(Ok(Either::Right(ValClos::UNIT)), &mut shell)
                .is_continue()
        );
        assert_eq!((machine.stack.len(), machine.call_site_frames), (2, 1));
        assert!(
            machine.state.is_err(),
            "settle restores the call site, then the value meets an Arg"
        );
        assert!(machine.step(&mut shell).is_continue());
        assert_eq!((machine.stack.len(), machine.call_site_frames), (1, 1));
        assert!(machine.step(&mut shell).is_continue());
        assert_eq!((machine.stack.len(), machine.call_site_frames), (0, 0));
        assert!(matches!(
            machine.step(&mut shell),
            ControlFlow::Break(Err(_))
        ));
        assert!(machine.pop_frame().is_none());
        assert!(machine.room(2, &shell).is_ok());
        assert_eq!(shell.local.audit.call_site(), &before);
    }

    #[test]
    fn panic_abandons_interleaved_call_site_and_computational_frames() {
        let mut shell = new_shell();
        let file =
            shell.install_script_context(&crate::source::SourceOrigin::named("<panic>"), "f g");
        let span = Some(Span::new(file, 0, 1));
        let before = shell.local.audit.call_site().clone();
        let panicking = ValClos::builtin(Arc::new(BuiltinEntry::new(
            Cow::Borrowed("machine-panic"),
            |_| {
                crate::typecheck::builtins::thunk(crate::typecheck::builtins::pure(
                    crate::ty::Ty::unit(),
                ))
            },
            "test-only panic",
            crate::builtin::BuiltinBody::Static(|_, _, _| panic!("abandon the continuation")),
        )));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = run(
                |machine, shell| {
                    machine.enter_call("f", span, shell);
                    machine.push_args(vec![ValClos::UNIT], span, shell).unwrap();
                    machine.enter_call("g", span, shell);
                    assert_eq!((machine.stack.len(), machine.call_site_frames), (3, 2));
                    Ok(Either::Left(force(panicking)?))
                },
                &Mooring::adrift(),
                &mut shell,
            );
        }));
        assert!(result.is_err());
        assert_eq!(shell.local.audit.call_site(), &before);
        assert_eq!(shell.local.machine_depth, 0);
    }

    /// The stack cap: its error text, and a refused push leaves
    /// `io.stdout` exactly where it was.
    #[test]
    fn stack_cap_refuses_and_leaves_stdout_untouched() {
        let mut shell = new_shell();
        // Sequential `;` costs no stack (the tail-call property), so the cap
        // needs genuine non-tail recursion: `let prev = f …` keeps this
        // call's `To` frame alive while its own recursive call runs.
        shell.session.stack_limit = 4;
        let before = matches!(shell.io.stdout, Sink::Stdout);
        let out = run_source(
            "let f = { |n| if $[$n <= 0] { return 0 } else { let prev = f $[$n - 1]; return $[$n + $prev] } }\nf 100",
            &mut shell,
        );
        match out {
            Err(Break::Error(e)) => {
                assert!(
                    matches!(&e.reason, Reason::State(f) if **f == StateFailure::Depth(DepthFailure { kind: DepthKind::EvalStack, limit: 4usize.into() })),
                    "got {:?}",
                    e.describe()
                );
            }
            other => panic!("expected a recursion-limit error, got {other:?}"),
        }
        assert_eq!(
            matches!(shell.io.stdout, Sink::Stdout),
            before,
            "a refused push must leave stdout untouched"
        );
    }

    #[test]
    fn map_assembly_needs_no_stack_frame() {
        let mut shell = new_shell();
        shell.session.stack_limit = 0;
        let node = Arc::new(Spanned::synthetic(CompKind::Prim(crate::ir::Prim::Map(
            Box::new([crate::ir::MapEntryV::Entry(
                Spanned::synthetic(Val::String("a".into())),
                Spanned::synthetic(Val::Int(1)),
            )]),
        ))));
        let out = evaluate(node, shell.env.clone(), &Mooring::adrift(), &mut shell);
        assert!(matches!(out.as_ref().map(ValClos::view), Ok(View::Map(_))));
    }

    #[test]
    fn map_validation_errors_name_the_operand_span() {
        use crate::ir::{MapEntryV, Prim};
        let mut shell = new_shell();
        let file = shell.install_script_context(
            &crate::source::SourceOrigin::named("<map>"),
            "                     ",
        );
        let span = Span::new(file, 10, 20);
        let bad = || Spanned::with_span(Some(span), Val::Int(1));
        for entry in [MapEntryV::Entry(bad(), bad()), MapEntryV::Spread(bad())] {
            let node = Arc::new(Spanned::synthetic(CompKind::Prim(Prim::Map(Box::new([
                entry,
            ])))));
            match evaluate(node, shell.env.clone(), &Mooring::adrift(), &mut shell) {
                Err(Break::Error(e)) => assert_eq!(
                    e.prov.as_ref().and_then(crate::shell::runtime::Prov::span),
                    Some(span)
                ),
                other => panic!("expected located map validation failure, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_bind_polls_before_its_rhs() {
        let mut shell = new_shell();
        let mooring = Mooring::adrift();
        shell.scope = Scope::clone(&mooring.home);
        let file = shell.install_script_context(
            &crate::source::SourceOrigin::named("<bind>"),
            "                     ",
        );
        let span = Span::new(file, 10, 20);
        let node = Arc::new(Spanned::with_span(
            Some(span),
            CompKind::bind(
                Pattern::Name(crate::ir::synthetic("map", 0)),
                Spanned::synthetic(CompKind::Return(Val::Variable("unbound".into()))),
                Spanned::synthetic(CompKind::Prim(crate::ir::Prim::Map(Box::new([])))),
            ),
        ));
        mooring
            .home
            .cancel(crate::process::cancel::CancelCause::Cancelled);
        let out = evaluate(node, shell.env.clone(), &mooring, &mut shell);
        assert!(
            matches!(out, Err(Break::Escape(Escape::Stopped(Stop { at: Some(at), .. }))) if at.span() == Some(span))
        );
    }

    #[test]
    fn map_operand_hoists_propagate_exit_without_running_later_operands() {
        let mut shell = new_shell();
        let out = run_source(
            "let k = 'a'; try { let _ = [$k: !{exit 5}, ...!{fail 'later'}]; return () } { |e| return () }",
            &mut shell,
        );
        assert!(matches!(out, Err(Break::Escape(Escape::Exit(5)))));
    }

    /// A cancelled `let f = { !f }; !f` returns rather than spinning: `Rec`
    /// is the one polled rule such a loop reaches.
    #[test]
    fn a_cancelled_tight_loop_returns() {
        let mut shell = new_shell();
        let mooring = Mooring::adrift();
        shell.scope = Scope::clone(&mooring.home);
        mooring
            .home
            .cancel(crate::process::cancel::CancelCause::Cancelled);
        let out = run_with("let f = { !$f }\n!$f", &mooring, &mut shell);
        assert!(
            out.is_err(),
            "a cancelled tight loop must halt rather than spin"
        );
    }

    /// A cancellation is a `Break` like any other: whichever polled rule
    /// sees it, the halt leaves `eval` through `locate` and so
    /// carries that node's span.
    #[test]
    fn a_cancelled_rec_and_a_cancelled_bind_both_carry_a_span() {
        for source in ["let f = { !$f }\n!$f", "if true { let x = 1; return $x }"] {
            let mut shell = new_shell();
            let mooring = Mooring::adrift();
            shell.scope = Scope::clone(&mooring.home);
            mooring
                .home
                .cancel(crate::process::cancel::CancelCause::Cancelled);
            match run_with(source, &mooring, &mut shell) {
                Err(Break::Escape(Escape::Stopped(Stop { at, .. }))) => {
                    assert!(at.is_some(), "a stop in `{source}` must be located");
                }
                other => panic!("`{source}` must halt when cancelled, got {other:?}"),
            }
        }
    }

    /// A native (`map`) applying a function that itself calls `map`
    /// nests one host stack frame's worth of `machine::run` per level.  That
    /// must fit a worker's stack up to `NESTED_MACHINE_LIMIT`, and stop there
    /// with the depth-exceeded error rather than a raw overflow — run on
    /// `std::thread::spawn`'s 2 MiB default, well under the 8 MiB
    /// `Shell::spawn_thread` gives a worker.
    #[test]
    fn nested_machines_fit_a_worker_stack() {
        let handle = std::thread::spawn(|| {
            let mut shell = new_shell();
            let out = run_source(
                "let f = { |n| if $[$n <= 0] { return 0 } else { \
                 let r = map $f [$[$n - 1]]; return $r[0] } }\n\
                 f 100000",
                &mut shell,
            );
            match out {
                Err(Break::Error(e)) => {
                    assert!(
                        matches!(&e.reason, Reason::State(failure) if matches!(**failure, StateFailure::Depth(DepthFailure { kind: DepthKind::NativeReentry, .. }))),
                        "expected the depth-exceeded error, got {:?}",
                        e.describe()
                    );
                }
                other => panic!("expected the native re-entry cap to fire, got {other:?}"),
            }
        });
        handle
            .join()
            .expect("nested map re-entry must not overflow a worker's own stack");
    }
    #[test]
    fn unchecked_machine_backstops_keep_invariant_state_and_apply_distinct() {
        let mut shell = new_shell();
        shell.local.machine_depth = NESTED_MACHINE_LIMIT;
        let Err(Break::Error(error)) = run(
            |_, _| panic!("depth check must precede seeding"),
            &Mooring::adrift(),
            &mut shell,
        ) else {
            panic!("depth refusal")
        };
        assert_eq!(
            error.reason,
            Reason::State(Box::new(StateFailure::Depth(DepthFailure {
                kind: DepthKind::NativeReentry,
                limit: NESTED_MACHINE_LIMIT.into()
            })))
        );
    }
    #[test]
    fn unchecked_condition_case_and_boundary_arity_are_typed() {
        let mut shell = new_shell();
        let node = CompKind::If {
            cond: Spanned::synthetic(Val::Int(1)),
            then: Spanned::synthetic(Val::Unit),
            else_: Spanned::synthetic(Val::Unit),
        };
        let Err(Break::Error(error)) = evaluate(
            Arc::new(Spanned::synthetic(node)),
            Env::new(),
            &Mooring::adrift(),
            &mut shell,
        ) else {
            panic!("invalid condition")
        };
        assert_eq!(
            error.reason,
            ValClos::int(1)
                .expected("if", Expected::Shape(ValueShape::Bool))
                .reason
        );
        let arms = [CaseArm {
            tag: Spanned::synthetic("other".into()),
            body: Spanned::synthetic(Val::Unit),
        }];
        let mooring = Mooring::adrift();
        let mut machine = Machine::new(&mooring);
        let Err(Break::Error(error)) = machine.step_case(
            &Spanned::synthetic(Val::Int(1)),
            &arms,
            None,
            Env::new(),
            &shell,
        ) else {
            panic!("invalid scrutinee")
        };
        assert_eq!(
            error.reason,
            ValClos::int(1)
                .expected("case", Expected::Shape(ValueShape::Variant))
                .reason
        );
        let scrutinee = Spanned::synthetic(Val::Variant {
            label: "tag".into(),
            payload: Box::new(Val::Unit),
        });
        let Err(Break::Error(error)) =
            machine.step_case(&scrutinee, &arms, None, Env::new(), &shell)
        else {
            panic!("missing arm")
        };
        assert_eq!(
            error.reason,
            Reason::Evaluation(Box::new(EvalFailure::CaseArm(CaseArmFailure {
                tag: "tag".into(),
                handled: vec!["other".into()]
            })))
        );
        let entry = &crate::shell::builtins::BOUNDARY_BUILTINS[0];
        let arity = entry.decl.fixed_arity();
        let Err(Break::Error(error)) = run_boundary(
            entry,
            &vec![ValClos::UNIT; arity + 1],
            None,
            None,
            &Mooring::adrift(),
            &mut shell,
        ) else {
            panic!("arity backstop")
        };
        assert!(
            matches!(error.reason, Reason::Invalid(f) if f.cause == InvalidCause::Arity(ArityFailure { expected: CountRule::Exactly(arity.into()), got: (arity + 1).into() }))
        );
    }
}
