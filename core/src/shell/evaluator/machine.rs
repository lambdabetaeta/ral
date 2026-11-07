//! The machine: a CEK machine for CBPV — closures in focus, frames on a
//! stack, one `step`.
//!
//! [`Machine::eval_rules`] has one match arm per `CompKind`;
//! [`Machine::step_return`] and [`Machine::step_halt`] are each frame's two
//! rules, one match arm per [`Frame`]. `step` itself only
//! dispatches on the shape of [`Focus`]. No arm calls another arm; no arm
//! loops. Every call pushes one `Arg` frame per argument, then forces.

use std::sync::Arc;

use crate::fact::{Failure, NotRunnable, Reason, Unrunnable};
use crate::io::{self, Sink};
use crate::ir::Name;
use crate::ir::Redirects;
use crate::ir::{
    Argv, CaseArm, CommandName, CommandWord, Comp, CompKind, GroupNode, NativeEntry, Pattern, Prim,
    Val, ValListElem, is_synthetic, synthetic,
};
use crate::process::Scope;
use crate::shell::builtins::concurrency::{self, legible_title};
use crate::shell::exec::dispatch::{self, Resolution};
use crate::shell::exec::pipeline;
use crate::shell::exec::redirect::scope::{RedirectState, WriteFate};
use crate::shell::grant::decode_capability_map;
use crate::shell::runtime::{
    Binding, Break, BroodOwner, BuiltinEntry, CapturePolicy, Closure, Cradle, DisplacedSite, Env,
    Error, FrameHandle, FrameName, HandlerArity, HandlerFrame, Mooring, ScopeOwner, Settled, Shell,
    Signature, TrailScope, Value, report_value, sig, sig_hint,
};
use crate::source::{Span, Spanned};
use crate::ty::Site;

use super::expr;
use super::pattern;
use super::scope::{WithinScope, WithinUndo};
use super::val::{form, spread_type_err};

// ── The stack ─────────────────────────────────────────────────────────

/// What a computation closure settles to, before a frame decides what it
/// means: a value, or a λ awaiting its arguments.
pub(crate) enum Terminal {
    Value(Value),
    /// `λx. M` awaiting an argument, over the environment it was reached in.
    Lambda {
        comp: Arc<Comp>,
        env: Env,
    },
}

/// The machine's one register.
pub(crate) enum Focus {
    /// `⟨M, ρ⟩` to step: `ρ` is the whole environment `M` was reached in.
    Eval { comp: Arc<Comp>, env: Env },
    /// A terminal meeting the frame above.
    Return(Terminal),
    /// A signal climbing the stack.
    Halt(Break),
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
        span: Option<Span>,
    },
    /// `□ V`: an argument awaiting the λ it binds; `span` is its call's.
    Arg {
        value: Value,
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
        handler: Value,
        span: Option<Span>,
    },
    Guard {
        cleanup: Value,
        span: Option<Span>,
    },
    /// Checks and wraps the returned value against the site (`Site::protect`).
    Protect {
        site: Arc<Site>,
        span: Option<Span>,
    },
    Cleanup {
        outcome: Settled<Value>,
        /// The current scope before the cleanup's shield, restored on pop
        /// and on abandon.
        prev: Scope,
    },
    Within(WithinUndo),
    /// Where its layer sits: a session frame pushed above survives the pop.
    Grant(usize),
    Audit {
        scope: TrailScope,
        span: Option<Span>,
    },
    /// `gather`: the unique owner of its body's brood, and the cradle to
    /// restore when it exits.
    Gather {
        owner: ScopeOwner,
        prev: Cradle,
        span: Option<Span>,
    },
}

/// Large frame states are boxed: any alone would exceed the cap.
const _: () = assert!(std::mem::size_of::<Frame>() <= 128);

/// The state: a focus and a stack. Private — nothing outside this module
/// constructs one or sees the stack.
pub(crate) struct Machine {
    focus: Focus,
    stack: Vec<Frame>,
    /// Provenance frames do not consume the computational stack budget.
    call_site_frames: usize,
}

impl Default for Machine {
    /// The one placeholder state: a `Unit` return over the empty stack.
    fn default() -> Self {
        Self {
            focus: Focus::Return(Terminal::Value(Value::Unit)),
            stack: Vec::new(),
            call_site_frames: 0,
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
        CompKind::Exec(_) | CompKind::App { .. } | CompKind::Force(_) => return None,
        CompKind::Prim(Prim::Binary(op, ..)) => op.symbol(),
        CompKind::Native { entry, .. } => return Some(entry.0.decl.name.clone().into()),
        CompKind::Prim(Prim::Negate(_)) => "-",
        CompKind::Prim(Prim::Not(_)) => "not",
        CompKind::Prim(Prim::Render(_)) => "str",
        CompKind::Prim(Prim::Concat(_)) => "concat",
        CompKind::Prim(Prim::Decode(_)) | CompKind::Protect { .. } => "decode",
        CompKind::Prim(Prim::Index { .. }) => "index",
        CompKind::Prim(Prim::Update { .. }) => "update",
        CompKind::Prim(Prim::List(_)) => "list",
        CompKind::Prim(Prim::Map(_)) => "map",
        CompKind::Return(_) => "return",
        CompKind::Lam { .. } => "lambda",
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
    };
    Some(FrameName::Static(name))
}

/// Protection preserves the source variable; compiler binders name no call.
fn forced_name(mut value: &Val) -> Option<&Name> {
    while let Val::Protected { value: inner, .. } = value {
        value = inner;
    }
    match value {
        Val::Variable(name) if !is_synthetic(name) => Some(name),
        _ => None,
    }
}

/// Unreachable for a checked program, Let-fun binding a function unrun
/// (SPEC §17.8); kept a clean error, not a panic.
fn bare_lambda_error() -> Error {
    Error::new(
        "tried to evaluate a bare lambda in computation position: a function value must be \
         applied to arguments or held as a thunk",
    )
    .with_hint(
        "either call this function with all its arguments, or hold it in a block with a \
         parameter, such as `{ |x| f 1 $x }`, to pass it as a value",
    )
}

/// `t as Value else Halt(bare lambda)` — the `F`-holed frames' shared move.
fn as_value(t: Terminal) -> Result<Value, Break> {
    match t {
        Terminal::Value(v) => Ok(v),
        Terminal::Lambda { .. } => Err(Break::Error(bare_lambda_error())),
    }
}

fn capture_overflowed() -> Error {
    Error::new(format!(
        "capture exceeded {} MiB and nothing was kept: a prefix is not what the command wrote",
        io::SINK_BUFFER_CAP / (1024 * 1024)
    ))
    .with_hint("did you mean to write this to a file? `cmd > out.txt` keeps every byte")
}

/// `V` meeting an argument: unreachable for a checked program, which applies
/// only what it has forced; kept a clean error, not a panic.
fn not_a_function(v: &Value) -> Break {
    sig_hint(
        format!("{} is not a function", v.type_name()),
        "the type checker forces every function before it is applied, so this is a fault \
         in ral rather than in your program: please report the source that produced it",
    )
}

fn rec_node(group: &Arc<GroupNode>, index: usize) -> Arc<Comp> {
    Arc::new(Spanned::synthetic(CompKind::Rec {
        group: Arc::clone(group),
        index,
    }))
}

/// A native's code, built afresh at each force: `λx₁…λxₙ. Native(b,
/// x₁…xₙ)`, located where it is forced. Its bodies have no span, so each is
/// located at the application that enters it.
fn native_code(entry: Arc<BuiltinEntry>, span: Option<Span>) -> Arc<Comp> {
    let params: Vec<_> = (0..entry.decl.fixed_arity())
        .map(|i| synthetic("native", i))
        .collect();
    let call = CompKind::Native {
        args: params.iter().cloned().map(Val::Variable).collect(),
        entry: NativeEntry(entry),
    };
    let code = params
        .into_iter()
        .rev()
        .fold(call, |body, param| CompKind::Lam {
            param,
            body: Arc::new(Spanned::synthetic(body)),
        });
    Arc::new(Spanned::with_span(span, code))
}

/// `body`, at `span` when it has no span of its own: such code, a native's
/// above all, runs where it was called.
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
    argv: &[Value],
    site: Option<&Arc<Site>>,
    span: Option<Span>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let arity = entry.decl.fixed_arity();
    if argv.len() != arity {
        let name = &entry.decl.name;
        return Err(sig(format!(
            "{name}: takes {arity} argument(s) in one call, got {}",
            argv.len()
        )));
    }
    super::call::run_native(entry, argv, site, span, mooring, shell)
}

fn form_args<'a>(
    args: impl IntoIterator<Item = &'a Val>,
    env: &Env,
    sig: &Signature,
) -> Result<Vec<Value>, Error> {
    args.into_iter().map(|arg| form(arg, env, sig)).collect()
}

pub(crate) fn close_argv(argv: &Argv, env: &Env, sig: &Signature) -> Result<Vec<Value>, Error> {
    let mut out = Vec::with_capacity(argv.len());
    for elem in argv {
        match elem {
            ValListElem::Single(v) => out.push(form(&v.item, env, sig)?),
            ValListElem::Spread(v) => match form(&v.item, env, sig)? {
                Value::List(list) => out.extend(list.iter().map(std::borrow::Cow::into_owned)),
                other => return Err(spread_type_err(&other)),
            },
        }
    }
    Ok(out)
}

pub(crate) fn close_redirects(
    redirects: &Redirects<Val>,
    env: &Env,
    sig: &Signature,
) -> Result<Redirects<String>, Error> {
    redirects.try_map(|v| form(v, env, sig).map(|v| v.to_string()))
}

/// A directory or plain file ran as a command.
fn names_no_program(b: &Break) -> bool {
    let Break::Error(e) = b else { return false };
    let Reason::Process(process) = &e.reason else {
        return false;
    };
    matches!(
        &process.failure,
        Failure::NotRunnable(NotRunnable {
            why: Unrunnable::Directory | Unrunnable::NotExecutable,
            ..
        })
    )
}

/// `b`, offering a path head given no arguments as text: it was most likely
/// meant as such.
fn offer_path_as_text(b: Break, head: &CommandWord, argv: &[Value]) -> Break {
    match (b, head.name()) {
        (Break::Error(e), path @ (CommandName::Path(_) | CommandName::TildePath(_)))
            if argv.is_empty() =>
        {
            Break::Error(e.append_hint(format!(
                "to use the path as text, write `#'{}'#`",
                path.written()
            )))
        }
        (b, _) => b,
    }
}

fn render_handler_args(name: &str, arity: HandlerArity, argv: &[Value]) -> Vec<Value> {
    let list = Value::list(
        Value::render_argv(argv)
            .into_iter()
            .map(Value::string)
            .collect(),
    );
    match arity {
        HandlerArity::CatchAll => vec![Value::string(name), list],
        HandlerArity::Unary => vec![list],
    }
}

/// Room for frames, handed out only after the stack cap check.
struct Slot<'a> {
    machine: &'a mut Machine,
    count: usize,
}

impl Slot<'_> {
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

impl Machine {
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
            return Err(sig_hint(
                format!(
                    "recursion limit exceeded ({} frames)",
                    shell.session.stack_limit
                ),
                "usually a runaway recursive function: \
                     raise via rc recursion-limit: or --recursion-limit",
            ));
        }
        Ok(())
    }

    fn reserve(&mut self, shell: &Shell) -> Settled<Slot<'_>> {
        self.reserve_n(1, shell)
    }

    fn reserve_n(&mut self, count: usize, shell: &Shell) -> Settled<Slot<'_>> {
        self.room(count, shell)?;
        Ok(Slot {
            machine: self,
            count,
        })
    }

    /// One `Arg` frame per argument, the first on top, where the head's
    /// first λ meets it.
    fn push_args(&mut self, args: Vec<Value>, span: Option<Span>, shell: &Shell) -> Settled<()> {
        self.reserve_n(args.len(), shell)?.extend(
            args.into_iter()
                .rev()
                .map(|value| Frame::Arg { value, span }),
        );
        Ok(())
    }

    /// One transition: `step_eval` on `Eval`, `step_return`/`step_halt` otherwise.
    fn step(&mut self, mooring: &Mooring, shell: &mut Shell) {
        let focus = std::mem::replace(&mut self.focus, Self::default().focus);
        self.focus = match focus {
            Focus::Eval { comp, env } => self.step_eval(&comp, env, mooring, shell),
            Focus::Return(t) => {
                let frame = self.pop_frame().expect(
                    "Return with an empty stack: evaluate's loop must stop before calling step again",
                );
                self.step_return(frame, t, mooring, shell)
            }
            Focus::Halt(brk) => {
                let frame = self.pop_frame().expect(
                    "Halt with an empty stack: evaluate's loop must stop before calling step again",
                );
                self.step_halt(frame, brk, mooring, shell)
            }
        }
        .unwrap_or_else(Focus::Halt);
    }

    // ── call, force ────────────────────────────────────────────────────

    fn enter_call(&mut self, name: impl Into<FrameName>, span: Option<Span>, shell: &mut Shell) {
        if let Some(prev) = shell.enter_frame(name, span)
            && !matches!(self.stack.last(), Some(Frame::CallSite(_)))
        {
            self.push_frame(Frame::CallSite(Box::new(prev)));
        }
    }

    /// The one way anything is called: `args` pushed, then `f` forced.
    fn call(
        &mut self,
        f: Value,
        args: Vec<Value>,
        span: Option<Span>,
        shell: &Shell,
    ) -> Settled<Focus> {
        self.push_args(args, span, shell)?;
        Self::force(f, span)
    }

    /// `force V` on a closed value: `force(thunk M) = M`, and a native's code
    /// built at `span`; nothing pushed.
    fn force(v: Value, span: Option<Span>) -> Settled<Focus> {
        match v {
            Value::Thunk(c) => {
                let (comp, env) = c.into_parts();
                Ok(Focus::Eval { comp, env })
            }
            Value::Native(entry) => Ok(Focus::Eval {
                comp: native_code(entry, span),
                env: Env::new(),
            }),
            other => Err(sig_hint(
                format!("cannot force {}: ! requires a Block", other.type_name()),
                "wrap in a block: !{ expr }",
            )),
        }
    }

    /// `force V` on a value still syntax: `force(thunk M) = M`, a literal block
    /// running in place and closing nothing.
    fn force_val(val: &Val, env: Env, span: Option<Span>, shell: &Shell) -> Settled<Focus> {
        match val {
            Val::Thunk(node) => Ok(Focus::Eval {
                comp: Arc::clone(node.shape()),
                env,
            }),
            other => Self::force(form(other, &env, &shell.sig)?, span),
        }
    }

    /// `push_redirect(redirs)`: nothing when empty, else install and push
    /// the `Redirect` frame.
    fn push_redirect(
        &mut self,
        redirs: &Redirects<String>,
        span: Option<Span>,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<()> {
        if redirs.is_empty() {
            return Ok(());
        }
        let slot = self.reserve(shell)?;
        let state = RedirectState::enter(redirs, span, mooring, shell)?;
        slot.push(Frame::Redirect(Box::new(state)));
        Ok(())
    }

    // ── Rules on a computation closure in focus ────────────────────────

    fn step_eval(
        &mut self,
        comp: &Arc<Comp>,
        env: Env,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Focus> {
        match operation_name(&comp.item) {
            Some(name) => shell.at_site(name.clone(), comp.span, |shell| {
                self.eval_rules(comp, env, mooring, shell)
                    .map_err(|b| b.locate(&name, comp.span, shell))
            }),
            None => self.eval_rules(comp, env, mooring, shell),
        }
    }

    /// Dispatch claims its own refusals; other operations use `step_eval`.
    fn eval_rules(
        &mut self,
        comp: &Arc<Comp>,
        env: Env,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Focus> {
        Ok(match &comp.item {
            CompKind::Protect { body, site } => {
                self.reserve(shell)?.push(Frame::Protect {
                    site: Arc::clone(site),
                    span: comp.span,
                });
                Focus::Eval {
                    comp: Arc::clone(body),
                    env,
                }
            }
            CompKind::Return(val) => Focus::Return(Terminal::Value(form(val, &env, &shell.sig)?)),

            // Canonical at A → C, never a value: the frame on top decides.
            // Unreachable except under `Arg`/a C-holed frame for a checked
            // program.
            CompKind::Lam { .. } => Focus::Return(Terminal::Lambda {
                comp: Arc::clone(comp),
                env,
            }),

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
                        rec_node(group, j)
                    };
                    (
                        name.clone(),
                        Binding {
                            value: Value::Thunk(Closure::captured(thunk_comp, restricted.clone())),
                            scheme: None,
                        },
                    )
                }));
                Focus::Eval {
                    comp: Arc::clone(&group.shape()[*index].1),
                    env: env2,
                }
            }

            CompKind::Tilde(path) => Focus::Return(Terminal::Value(Value::string(
                path.shown(&shell.context.require_home()?),
            ))),

            CompKind::Force(val) => {
                let name = forced_name(val).map_or(FrameName::Static("!"), FrameName::from);
                self.enter_call(name.clone(), comp.span, shell);
                Self::force_val(val, env, comp.span, shell)
                    .map_err(|b| b.locate(&name, comp.span, shell))?
            }

            CompKind::Prim(prim) => {
                Focus::Return(Terminal::Value(expr::eval_prim(prim, env, shell)?))
            }

            CompKind::Bind {
                comp: rhs,
                pattern: _,
                rest: _,
            } => {
                shell.poll()?;
                self.reserve(shell)?.push(Frame::To {
                    bind: Arc::clone(comp),
                    env: env.clone(),
                });
                Focus::Eval {
                    comp: Arc::clone(rhs),
                    env,
                }
            }

            CompKind::Seq { first, rest } => {
                shell.poll()?;
                self.reserve(shell)?.push(Frame::Seq {
                    rest: Arc::clone(rest),
                    env: env.clone(),
                    span: comp.span,
                });
                Focus::Eval {
                    comp: Arc::clone(first),
                    env,
                }
            }

            CompKind::If { cond, then, else_ } => match form(&cond.item, &env, &shell.sig)? {
                Value::Bool(b) => {
                    Self::force_val(&if b { then } else { else_ }.item, env, comp.span, shell)?
                }
                other => {
                    return Err(sig(format!(
                        "if: expected Bool, got {} '{}'",
                        other.type_name(),
                        other
                    )));
                }
            },

            CompKind::Case { scrutinee, arms } => {
                self.step_case(scrutinee, arms, comp.span, env, shell)?
            }

            CompKind::App { head, args } => {
                let name = match &head.item {
                    CompKind::Force(val) => forced_name(val),
                    _ => None,
                }
                .map_or(FrameName::Static("apply"), FrameName::from);
                self.enter_call(name.clone(), comp.span, shell);
                let call = (|| {
                    shell.poll()?;
                    let args = form_args(args.iter().map(|arg| &arg.item), &env, &shell.sig)?;
                    self.push_args(args, comp.span, shell)?;
                    Ok(Focus::Eval {
                        comp: Arc::clone(head),
                        env,
                    })
                })();
                call.map_err(|b: Break| b.locate(&name, comp.span, shell))?
            }

            CompKind::Exec(exec) => self.step_exec(exec, comp.span, &env, mooring, shell)?,

            CompKind::Native { entry, args } => {
                let args = form_args(args, &env, &shell.sig)?;
                let v = super::call::run_native(&entry.0, &args, None, comp.span, mooring, shell)?;
                Focus::Return(Terminal::Value(v))
            }

            CompKind::Pipeline { stages } => Focus::Return(Terminal::Value(
                pipeline::PipeNode::run(stages, &env, STAGE_EVAL, mooring, shell)?,
            )),

            CompKind::Capture(body) => {
                let slot = self.reserve(shell)?;
                let (sink, buf) = io::new_buffer();
                let prev = std::mem::replace(&mut shell.io.stdout, sink);
                slot.push(Frame::Capture {
                    prev,
                    buf,
                    span: comp.span,
                });
                Focus::Eval {
                    comp: Arc::clone(body),
                    env,
                }
            }

            CompKind::Redirect { body, redirects } => {
                let redirs = close_redirects(redirects, &env, &shell.sig)?;
                self.push_redirect(&redirs, comp.span, mooring, shell)?;
                Focus::Eval {
                    comp: Arc::clone(body),
                    env,
                }
            }

            CompKind::Within {
                opts,
                handlers,
                body,
            } => {
                let opts = form(&Val::Record(Arc::clone(opts)), &env, &shell.sig)?;
                let arms = handlers
                    .as_ref()
                    .map(|arms| {
                        arms.iter()
                            .map(|arm| {
                                Ok((arm.name.clone(), form(&arm.value.item, &env, &shell.sig)?))
                            })
                            .collect::<Settled<Vec<_>>>()
                    })
                    .transpose()?;
                let scope = WithinScope::parse(&opts.as_map("within")?, arms, shell)?;
                let body = form(body, &env, &shell.sig)?;
                let slot = self.reserve(shell)?;
                slot.push(Frame::Within(scope.enter(shell)));
                Self::force(body, comp.span)?
            }

            CompKind::Grant { caps, body } => {
                let c = form(&Val::Record(Arc::clone(caps)), &env, &shell.sig)?;
                let caps = decode_capability_map(&c, "grant", &shell.freeze_ctx())?;
                let body = form(body, &env, &shell.sig)?;
                let slot = self.reserve(shell)?;
                let at = shell.context.grants.len();
                shell.context.grants.push(caps);
                shell.audit_deputy_prefixes();
                slot.push(Frame::Grant(at));
                Self::force(body, comp.span)?
            }

            CompKind::Try { body, handler } => {
                let body = form(body, &env, &shell.sig)?;
                let handler = form(handler, &env, &shell.sig)?;
                self.reserve(shell)?.push(Frame::Try {
                    handler,
                    span: comp.span,
                });
                Self::force(body, comp.span)?
            }

            CompKind::Guard { body, cleanup } => {
                let body = form(body, &env, &shell.sig)?;
                let cleanup = form(cleanup, &env, &shell.sig)?;
                self.reserve(shell)?.push(Frame::Guard {
                    cleanup,
                    span: comp.span,
                });
                Self::force(body, comp.span)?
            }

            CompKind::Audit { body } => {
                let body = form(body, &env, &shell.sig)?;
                let slot = self.reserve(shell)?;
                slot.push(Frame::Audit {
                    scope: shell.local.audit.open(CapturePolicy::Bytes),
                    span: comp.span,
                });
                Self::force(body, comp.span)?
            }

            CompKind::Spawn { title, body } => {
                let title = title
                    .as_ref()
                    .map(|t| legible_title(&form(&t.item, &env, &shell.sig)?, "spawn"))
                    .transpose()?;
                let body = form(body, &env, &shell.sig)?;
                Focus::Return(Terminal::Value(concurrency::spawn_form(
                    title, &body, mooring, shell,
                )?))
            }

            CompKind::Gather { body } => {
                let body = form(body, &env, &shell.sig)?;
                let slot = self.reserve(shell)?;
                let owner = ScopeOwner::new(BroodOwner::Gather);
                let prev = std::mem::replace(
                    &mut shell.local.cradle,
                    Cradle::owning(shell.scope.clone(), owner.brood()),
                );
                slot.push(Frame::Gather {
                    owner,
                    prev,
                    span: comp.span,
                });
                Self::force(body, comp.span)?
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
    ) -> Settled<Focus> {
        let (label, payload) = match form(&scrutinee.item, &env, &shell.sig)? {
            Value::Variant { label, payload } => (label, payload),
            other => {
                return Err(sig(format!(
                    "case: scrutinee must be a variant, got {} {}",
                    other.type_name(),
                    other
                )));
            }
        };
        let Some(arm) = arms.iter().find(|arm| arm.tag.item == *label) else {
            let handled: Vec<String> = arms
                .iter()
                .map(|a| format!("{}{}", crate::ty::TAG_PREFIX, a.tag.item))
                .collect();
            return Err(sig(format!(
                "case: no arm for variant `{label}`; this case matches: {}",
                handled.join(", ")
            )));
        };
        let payload = payload.map_or(Value::Unit, |p| *p);
        self.push_args(vec![payload], span, shell)?;
        Self::force_val(&arm.body.item, env, span, shell)
    }

    fn step_exec(
        &mut self,
        exec: &crate::ir::Exec,
        span: Option<Span>,
        env: &Env,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Focus> {
        let name = FrameName::from(exec.head.name().written().into_owned());
        self.enter_call(name.clone(), span, shell);
        self.dispatch_exec(exec, span, env, mooring, shell)
            .map_err(|b| b.locate(&name, span, shell))
    }

    fn dispatch_exec(
        &mut self,
        exec: &crate::ir::Exec,
        span: Option<Span>,
        env: &Env,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Focus> {
        shell.poll()?;
        let argv = close_argv(&exec.argv, env, &shell.sig)?;
        let redirs = close_redirects(&exec.redirects, env, &shell.sig)?;
        let offer = |b| offer_path_as_text(b, &exec.head, &argv);
        match dispatch::classify_command(&exec.head, env, mooring, shell).map_err(offer)? {
            Resolution::Env(Value::Native(entry)) if entry.decl.is_boundary() => {
                self.push_redirect(&redirs, span, mooring, shell)?;
                let v = run_boundary(&entry, &argv, exec.site.as_ref(), span, mooring, shell)?;
                Ok(Focus::Return(Terminal::Value(v)))
            }
            // Agda's `prim` rule: a builtin given its arity runs on its argv.
            Resolution::Env(Value::Native(entry)) if argv.len() == entry.decl.fixed_arity() => {
                self.push_redirect(&redirs, span, mooring, shell)?;
                let v = super::call::run_native(&entry, &argv, None, span, mooring, shell)?;
                Ok(Focus::Return(Terminal::Value(v)))
            }
            Resolution::Env(v) => {
                self.push_redirect(&redirs, span, mooring, shell)?;
                self.call(v, argv, span, shell)
            }
            Resolution::Handler { entry, depth } => {
                self.push_redirect(&redirs, span, mooring, shell)?;
                let slot = self.reserve(shell)?;
                let frame = Box::new(shell.context.handlers.strip_matched(depth));
                slot.push(Frame::Unmask { frame });
                let args = render_handler_args(&entry.name, entry.arity, &argv);
                self.call(entry.thunk.clone(), args, span, shell)
            }
            Resolution::Base(entry) => {
                let v = dispatch::run_base_frame(&entry, &argv, &redirs, span, mooring, shell)?;
                Ok(Focus::Return(Terminal::Value(v)))
            }
            Resolution::External(head) => {
                let v = dispatch::run_external(head, &argv, &redirs, span, mooring, shell)
                    .map_err(|b| if names_no_program(&b) { offer(b) } else { b })?;
                Ok(Focus::Return(Terminal::Value(v)))
            }
        }
    }

    // ── Frames, and their two rules each ───────────────────────────────

    fn step_return(
        &mut self,
        frame: Frame,
        t: Terminal,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Focus> {
        match frame {
            Frame::CallSite(prev) => {
                shell.local.audit.restore_site(*prev);
                Ok(Focus::Return(t))
            }
            Frame::Protect { site, span } => shell.at_site("decode", span, |shell| {
                let value = as_value(t).and_then(|v| site.protect(v, shell));
                value
                    .map(|v| Focus::Return(Terminal::Value(v)))
                    .map_err(|b| b.locate("decode", span, shell))
            }),
            Frame::To { bind, env } => {
                let v = as_value(t).map_err(|b| b.locate("let", bind.span, shell))?;
                let CompKind::Bind { pattern, rest, .. } = &bind.item else {
                    unreachable!("a To frame's `bind` is always a Bind comp")
                };
                let env = pattern::bind(pattern, &v, env)
                    .map_err(|b| b.locate("let", bind.span, shell))?;
                Ok(Focus::Eval {
                    comp: Arc::clone(rest),
                    env,
                })
            }

            Frame::Seq { rest, env, span } => {
                as_value(t).map_err(|b| b.locate("sequence", span, shell))?;
                Ok(Focus::Eval { comp: rest, env })
            }

            // β: `⟨λx. M, ρ⟩` meeting `Arg(V)` is `⟨M, ρ[x ↦ V]⟩`.
            Frame::Arg { value, span } => match t {
                Terminal::Lambda { comp, mut env } => {
                    let CompKind::Lam { param, body } = &comp.item else {
                        unreachable!("a Terminal::Lambda is built only from a Lam node")
                    };
                    shell.poll().map_err(|b| b.locate("apply", span, shell))?;
                    env.bind(
                        param.clone(),
                        Binding {
                            value,
                            scheme: None,
                        },
                    );
                    Ok(Focus::Eval {
                        comp: located(body, span),
                        env,
                    })
                }
                Terminal::Value(v) => Err(not_a_function(&v).locate("apply", span, shell)),
            },

            Frame::Capture { prev, buf, span } => {
                shell.io.stdout = prev;
                if buf.overflowed() {
                    Err(Break::from(capture_overflowed()).locate("capture", span, shell))
                } else {
                    Ok(Focus::Return(Terminal::Value(Value::bytes(buf.take()))))
                }
            }

            Frame::Redirect(state) => {
                let mut state = *state;
                state.leave(WriteFate::Commit, mooring, shell)?;
                Ok(Focus::Return(t))
            }

            Frame::Try { .. } => Ok(Focus::Return(t)),

            Frame::Guard { cleanup, span } => {
                let v = as_value(t).map_err(|b| b.locate("guard", span, shell))?;
                let slot = self
                    .reserve(shell)
                    .map_err(|b| b.locate("guard", span, shell))?;
                let prev = shell.shield(mooring.cleanup_grace);
                slot.push(Frame::Cleanup {
                    outcome: Ok(v),
                    prev,
                });
                Self::force(cleanup, span).map_err(|b| b.locate("guard", span, shell))
            }

            Frame::Cleanup { outcome, prev } => {
                shell.scope = prev;
                outcome.map(|v| Focus::Return(Terminal::Value(v)))
            }

            undo @ (Frame::Unmask { .. } | Frame::Within(_) | Frame::Grant(_)) => {
                undo.abandon(shell);
                Ok(Focus::Return(t))
            }

            Frame::Audit { scope, span } => {
                let trail = shell.local.audit.close(scope);
                let v = as_value(t).map_err(|b| b.locate("audit", span, shell))?;
                Ok(Focus::Return(Terminal::Value(report_value(Ok(v), trail))))
            }

            Frame::Gather { owner, prev, span } => {
                shell.local.cradle = prev;
                let v = concurrency::exit_brood(&owner, as_value(t), mooring.cleanup_grace, shell)
                    .map_err(|b| b.locate("gather", span, shell))?;
                Ok(Focus::Return(Terminal::Value(v)))
            }
        }
    }

    fn step_halt(
        &mut self,
        frame: Frame,
        s: Break,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Focus> {
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
                        Break::Error(e.abandoning_tail())
                    }
                    s => s,
                })
            }

            Frame::Seq { .. } => Err(match s {
                Break::Error(e) => Break::Error(e.abandoning_tail()),
                s @ Break::Escape(_) => s,
            }),

            Frame::Capture { prev, buf, .. } => {
                shell.io.stdout = prev;
                Err(match s {
                    Break::Error(e) if buf.total() > 0 => {
                        Break::Error(e.discarded(u32::try_from(buf.total()).unwrap_or(u32::MAX)))
                    }
                    s => s,
                })
            }

            Frame::Arg { .. } | Frame::Protect { .. } => Err(s),

            Frame::Cleanup { prev, .. } => {
                shell.scope = prev;
                Err(s)
            }

            Frame::Redirect(state) => {
                let mut state = *state;
                let _ = state.leave(WriteFate::Abort, mooring, shell);
                Err(s)
            }

            undo @ (Frame::Unmask { .. } | Frame::Within(_) | Frame::Grant(_)) => {
                undo.abandon(shell);
                Err(s)
            }

            Frame::Try { handler, span } => match s {
                Break::Error(e) => {
                    let record = Value::from_datum(e.record(shell));
                    self.call(handler, vec![record], span, shell)
                        .map_err(|b| b.locate("try", span, shell))
                }
                escape @ Break::Escape(_) => Err(escape),
            },

            Frame::Guard { cleanup, span } => {
                let slot = self
                    .reserve(shell)
                    .map_err(|b| b.locate("guard", span, shell))?;
                let prev = shell.shield(mooring.cleanup_grace);
                slot.push(Frame::Cleanup {
                    outcome: Err(s),
                    prev,
                });
                Self::force(cleanup, span).map_err(|b| b.locate("guard", span, shell))
            }

            Frame::Audit { scope, .. } => match s {
                Break::Error(e) => {
                    let trail = shell.local.audit.close(scope);
                    let record = e.record(shell);
                    Ok(Focus::Return(Terminal::Value(report_value(
                        Err(record),
                        trail,
                    ))))
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
    /// `Grant` removes its layer; `Gather` restores the cradle and drops its
    /// owner to stop its brood without waiting.  The rest do nothing.
    fn abandon(self, shell: &mut Shell) {
        match self {
            Self::CallSite(prev) => shell.local.audit.restore_site(*prev),
            Self::Capture { prev, .. } => shell.io.stdout = prev,
            Self::Redirect(state) => state.abandon(shell),
            Self::Unmask { frame } => shell.context.handlers.restore_matched(*frame),
            Self::Audit { scope, .. } => drop(shell.local.audit.close(scope)),
            Self::Within(undo) => undo.apply(shell),
            Self::Grant(at) => shell.context.grants.remove(at, 1),
            Self::Cleanup { prev, .. } => shell.scope = prev,
            Self::Gather { prev, .. } => shell.local.cradle = prev,
            Self::To { .. }
            | Self::Seq { .. }
            | Self::Arg { .. }
            | Self::Try { .. }
            | Self::Guard { .. }
            | Self::Protect { .. } => {}
        }
    }
}

// ── Boundaries ───────────────────────────────────────────────────────────

/// `Return(Lambda(_))` at the end of a run: unreachable for a checked
/// program, kept as a clean error rather than a panic.
fn end_of_run(focus: Focus) -> Settled<Value> {
    match focus {
        Focus::Return(Terminal::Value(v)) => Ok(v),
        Focus::Return(Terminal::Lambda { .. }) => Err(Break::Error(bare_lambda_error())),
        Focus::Halt(s) => Err(s),
        Focus::Eval { .. } => {
            unreachable!(
                "the loop only stops when the stack is empty and focus is a terminal or a halt"
            )
        }
    }
}

/// The nested-machine depth guard around `seed`, then `step` until the stack
/// is empty: `Ok` on a value, `Err` on a halt. `catch_unwind` around the
/// loop; on panic, `abandon` walks every frame top-down before resuming.
fn run(
    seed: impl FnOnce(&mut Machine, &Mooring, &mut Shell),
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    if shell.local.machine_depth >= NESTED_MACHINE_LIMIT {
        return Err(sig_hint(
            format!("native re-entry depth exceeded ({NESTED_MACHINE_LIMIT})"),
            "a builtin such as `map` applied a function that itself applied `map`, too deep: \
                 restructure with a tail-recursive loop",
        ));
    }
    shell.local.machine_depth += 1;
    let mut m = Machine::default();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        seed(&mut m, mooring, shell);
        while !(m.stack.is_empty() && matches!(m.focus, Focus::Return(_) | Focus::Halt(_))) {
            m.step(mooring, shell);
        }
    }));
    shell.local.machine_depth -= 1;
    match result {
        Ok(()) => end_of_run(m.focus),
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
) -> Settled<Value> {
    run(
        |m, _, _| m.focus = Focus::Eval { comp, env },
        mooring,
        shell,
    )
}

/// The same, from a closed value meeting arguments: [`Machine::call`] sets
/// the first state, so a value meeting none is forced.
pub(crate) fn apply(
    f: Value,
    args: Vec<Value>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    run(
        |m, _, shell| m.focus = m.call(f, args, None, shell).unwrap_or_else(Focus::Halt),
        mooring,
        shell,
    )
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::ir::{Name, Phrase};
    use crate::shell::evaluator::{run_source, with_capture};
    use crate::shell::runtime::{Break, Escape, Stop, captured};

    /// Run every phrase of `source` through the machine: a `Define` binds
    /// into the running `Env` with `pattern::bind`, a `Run` phrase's
    /// value is the previous one, mirroring `run_phrases`'s shape without
    /// its lease machinery, irrelevant to what these tests probe.
    fn run_with(source: &str, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
        let file =
            shell.install_script_context(&crate::source::SourceOrigin::named("<machine>"), source);
        let top = crate::compile::compile_and_typecheck(
            source,
            crate::test_helper::core_schemes(),
            file,
            None,
            None,
        )
        .expect("compile");
        let mut env = shell.env.clone();
        let mut value = Ok(Value::Unit);
        for phrase in &top.phrases {
            match &phrase.item {
                Phrase::Run(comp) => {
                    value = evaluate(comp.clone(), env.clone(), mooring, shell);
                    value.as_ref().map_err(Clone::clone)?;
                }
                Phrase::Define { pattern, comp, .. } => {
                    let v = evaluate(comp.clone(), env.clone(), mooring, shell)?;
                    env = pattern::bind(pattern, &v, env)?;
                    value = Ok(Value::Unit);
                }
            }
        }
        shell.env = env;
        value
    }

    fn new_shell() -> Shell {
        crate::test_helper::core_shell()
    }

    /// The hint `source` fails with, when it fails.
    #[cfg(unix)]
    fn failure_hint(source: &str, shell: &mut Shell) -> Option<String> {
        match run_source(source, shell) {
            Err(Break::Error(e)) => e.hint.as_deref().map(str::to_owned),
            other => panic!("expected an error, got {other:?}"),
        }
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
        let hint = failure_hint(&dir, &mut shell).expect("a hint");
        let (grant, text) = hint.rsplit_once("\n\n").expect("both hints are kept");
        assert!(text.contains(&format!("{AS_TEXT} `#'{dir}'#`")), "{hint}");
        assert!(grant.contains("grant") && !grant.contains("#'"), "{hint}");
        let hint = failure_hint(&format!("{dir} x"), &mut shell).expect("a hint");
        assert!(!hint.contains(AS_TEXT), "{hint}");
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
            matches!(out, Ok(Value::Int(0))),
            "the block's `inner` must not outlive it, got {out:?}"
        );
    }

    /// A `let` captures the whole command that produces its value.
    #[test]
    fn a_let_captures_the_command_that_produces_its_value() {
        let mut shell = new_shell();
        let (result, bytes, _overflowed) = with_capture(&mut shell, |shell| {
            run_source("let x = !{ echo a; echo b }\nreturn $x", shell)
        });
        assert!(matches!(result, Ok(Value::String(ref s)) if s.as_str() == "a\nb"));
        assert!(bytes.is_empty(), "nothing escaped the capture");
    }

    /// A stand-in writes as the command it stands in for does, all of it, so a
    /// capture of the command takes every statement of the stand-in.
    #[test]
    fn a_captured_stand_ins_every_write_is_captured() {
        let mut shell = new_shell();
        let (result, bytes, _overflowed) = with_capture(&mut shell, |shell| {
            run_source(
                "alias curl { |a| echo note; echo body }\nlet x = curl\nreturn $x",
                shell,
            )
        });
        assert!(matches!(result, Ok(Value::String(ref s)) if s.as_str() == "note\nbody"));
        assert!(bytes.is_empty(), "nothing escaped the capture");
    }

    /// A capture that does not become its value commits nothing: what it
    /// wrote before failing is dropped, not written to the sink it replaced.
    #[test]
    fn a_failing_capture_writes_nothing_to_the_sink_it_replaced() {
        let mut shell = new_shell();
        let (result, bytes, _overflowed) = with_capture(&mut shell, |shell| {
            run_source(
                "alias boom { |a| echo partial; fail 'x' }\n\
                 let x = boom\nreturn $x",
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
        let (result, bytes, _overflowed) = with_capture(&mut shell, |shell| {
            run_source(
                "alias boom { |a| echo partial; fail 'x' }\n\
                 let x = try { return !{ echo in | boom } } { |_| return 'handled' }\n\
                 return $x",
                shell,
            )
        });
        assert!(matches!(result, Ok(Value::String(ref s)) if s.as_str() == "handled"));
        assert!(bytes.is_empty(), "the failed capture wrote {bytes:?}");
    }

    #[test]
    fn nested_failing_captures_sum_their_bytes_into_one_note() {
        let mut shell = new_shell();
        let (result, bytes, _) = with_capture(&mut shell, |shell| {
            run_source(
                "alias boom { |a| echo cde; fail 'x' }\n\
                 let x = !{ echo ab; let y = boom; echo z }\nreturn $x",
                shell,
            )
        });
        assert!(bytes.is_empty(), "the failed captures wrote {bytes:?}");
        let Err(Break::Error(e)) = result else {
            panic!("expected the inner failure, got {result:?}")
        };
        assert_eq!(e.unwound.discarded, 7, "ab\\n plus cde\\n");
        assert!(e.hint.is_none());
        let rendered = e.render(shell.sources(), None, shell.exit_hints());
        assert_eq!(rendered.matches("were discarded").count(), 1, "{rendered}");
        assert!(rendered.contains("7 bytes written to stdout"), "{rendered}");
    }

    #[test]
    fn discard_and_abandoned_tail_notes_render_in_that_order() {
        let mut shell = new_shell();
        let (result, bytes, _) = with_capture(&mut shell, |shell| {
            run_source(
                "let x = !{ echo ab; fail 'x'; echo later }; return $x",
                shell,
            )
        });
        assert_eq!(bytes, [] as [u8; 0]);
        let Err(Break::Error(e)) = result else {
            panic!("expected failure")
        };
        assert_eq!(e.unwound.discarded, 3);
        assert!(e.unwound.abandoned_tail);
        assert!(e.hint.is_none());
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
        let (result, bytes, _overflowed) = with_capture(&mut shell, |shell| {
            run_source(
                "alias boom { |a| echo partial; exit 3 }\nlet x = boom\nreturn $x",
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
        let app = Spanned::with_span(Some(span), CompKind::App { head, args });
        let out = evaluate(
            Arc::new(app),
            shell.env.clone(),
            &Mooring::adrift(),
            &mut shell,
        );
        match out {
            Err(Break::Error(e)) => {
                assert!(
                    e.describe().contains("is not a function"),
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

    /// A curried call over a thunked partial application: `g 2` with
    /// `g = thunk (f 1)`.
    #[test]
    fn curried_call_over_a_thunked_partial_application() {
        let mut shell = new_shell();
        let out = run_source(
            "let f = { |a b| return $[$a + $b] }\nlet g = f 1\ng 2",
            &mut shell,
        )
        .expect("curried call must succeed");
        assert!(matches!(out, Value::Int(3)), "expected 3, got {out:?}");
    }

    /// `apply` — the boundary natives reach `machine::apply` through —
    /// meeting a closed lambda value directly, with no `Comp` in sight.
    #[test]
    fn apply_meets_a_closed_lambda_value() {
        let mut shell = new_shell();
        let f = run_source("{ |a b| return $[$a + $b] }", &mut shell).expect("closure must close");
        let mooring = Mooring::adrift();
        let out = apply(f, vec![Value::Int(1), Value::Int(2)], &mooring, &mut shell)
            .expect("apply must succeed");
        assert!(matches!(out, Value::Int(3)), "expected 3, got {out:?}");
    }

    /// A two-member mutually recursive group.
    #[test]
    fn two_member_recursive_group() {
        let mut shell = new_shell();
        let out = run_source(
            "let even = { |n| if $[$n == 0] { return true } else { odd $[$n - 1] } }\n\
             let odd = { |n| if $[$n == 0] { return false } else { even $[$n - 1] } }\n\
             even 10",
            &mut shell,
        )
        .expect("mutual recursion must terminate");
        assert!(
            matches!(out, Value::Bool(true)),
            "expected true, got {out:?}"
        );
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
        shell.env.bind(
            "k".into(),
            Binding {
                value: Value::Int(1),
                scheme: None,
            },
        );

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

        let mut machine = Machine::default();
        let Focus::Eval { env: env1, .. } = machine
            .eval_rules(&rec_even, shell.env.clone(), &mooring, &mut shell)
            .expect("first unfold")
        else {
            panic!("expected an Eval focus");
        };
        let Value::Thunk(odd1) = env1.get("odd").cloned().expect("odd bound") else {
            panic!("odd must be a thunk");
        };
        let forced_env = odd1.env().clone();

        let Focus::Eval { env: env2, .. } = machine
            .eval_rules(odd1.comp(), forced_env.clone(), &mooring, &mut shell)
            .expect("second unfold")
        else {
            panic!("expected an Eval focus");
        };
        let Value::Thunk(even2) = env2.get("even").cloned().expect("even bound") else {
            panic!("even must be a thunk");
        };
        assert!(
            even2.env().ptr_eq(&forced_env),
            "a later unfold's siblings hold the environment that forced it, unchanged"
        );

        // A group of one: its own member is the node already in focus.
        let self_body = Arc::new(Spanned::synthetic(CompKind::Return(Val::Unit)));
        let solo = GroupNode::new(Box::from([(Name::from("f"), self_body)]));
        let rec_solo = Arc::new(Spanned::synthetic(CompKind::Rec {
            group: Arc::clone(&solo),
            index: 0,
        }));
        let Focus::Eval { env: solo_env, .. } = machine
            .eval_rules(&rec_solo, shell.env.clone(), &mooring, &mut shell)
            .expect("solo unfold")
        else {
            panic!("expected an Eval focus");
        };
        let Value::Thunk(solo_closure) = solo_env.get("f").cloned().expect("f bound") else {
            panic!("f must be a thunk");
        };
        assert!(
            Arc::ptr_eq(solo_closure.comp(), &rec_solo),
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
        let Value::Map(record) = &out else {
            panic!("expected a record, got {out:?}")
        };
        assert!(
            record.literal_env().is_some(),
            "a literal forms as a value closure, not built data"
        );

        let expected = Value::map(vec![
            (
                "b".to_string(),
                Value::list(vec![Value::Int(1), Value::Int(2)]),
            ),
            ("a".to_string(), Value::Int(1)),
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

        let Value::List(inner) = record.get("b").expect("a list field").into_owned() else {
            panic!("expected a list")
        };
        assert!(
            inner.literal_env().is_some(),
            "a nested list literal also forms as a value closure"
        );

        let out = run_source("let l = [1, 2]\nreturn $l", &mut shell)
            .expect("a literal naming nothing must still form");
        let Value::List(list) = &out else {
            panic!("expected a list, got {out:?}")
        };
        let env = list
            .literal_env()
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
            matches!(out, Value::Bool(true)),
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
        let mut machine = Machine::default();
        machine.enter_call("f", span, &mut shell);
        machine.enter_call("g", span, &mut shell);
        assert_eq!((machine.stack.len(), machine.call_site_frames), (1, 1));
        machine
            .push_args(vec![Value::Unit, Value::Unit], span, &shell)
            .expect("provenance costs no computational frames");
        machine.enter_call("f", span, &mut shell);
        machine.enter_call("g", span, &mut shell);
        assert_eq!((machine.stack.len(), machine.call_site_frames), (4, 2));
        assert!(machine.reserve(&shell).is_err());
        assert_eq!((machine.stack.len(), machine.call_site_frames), (4, 2));

        let mooring = Mooring::adrift();
        machine.step(&mooring, &mut shell);
        assert_eq!((machine.stack.len(), machine.call_site_frames), (3, 1));
        assert!(matches!(machine.focus, Focus::Return(_)));
        machine.step(&mooring, &mut shell);
        assert_eq!((machine.stack.len(), machine.call_site_frames), (2, 1));
        assert!(matches!(machine.focus, Focus::Halt(_)));
        machine.step(&mooring, &mut shell);
        assert_eq!((machine.stack.len(), machine.call_site_frames), (1, 1));
        machine.step(&mooring, &mut shell);
        assert_eq!((machine.stack.len(), machine.call_site_frames), (0, 0));
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
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = run(
                |machine, _, shell| {
                    machine.enter_call("f", span, shell);
                    machine.push_args(vec![Value::Unit], span, shell).unwrap();
                    machine.enter_call("g", span, shell);
                    assert_eq!((machine.stack.len(), machine.call_site_frames), (3, 2));
                    panic!("abandon the continuation");
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
        let before = matches!(shell.io.stdout, Sink::Terminal);
        let out = run_source(
            "let f = { |n| if $[$n <= 0] { return 0 } else { let prev = f $[$n - 1]; return $[$n + $prev] } }\nf 100",
            &mut shell,
        );
        match out {
            Err(Break::Error(e)) => {
                assert!(
                    e.describe().contains("recursion limit exceeded"),
                    "got {:?}",
                    e.describe()
                );
            }
            other => panic!("expected a recursion-limit error, got {other:?}"),
        }
        assert_eq!(
            matches!(shell.io.stdout, Sink::Terminal),
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
        assert!(matches!(out, Ok(Value::Map(_))));
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
    /// sees it, the halt leaves `step_eval` through `locate` and so
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
                        e.describe().contains("native re-entry depth exceeded"),
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
}
