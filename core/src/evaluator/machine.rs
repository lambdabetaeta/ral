//! The machine: closures in focus, frames on a stack, one `step`.
//!
//! [`Machine::eval_rules`] has one match arm per `CompKind`;
//! [`Machine::step_return`] and [`Machine::step_halt`] are each frame's two
//! rules, one match arm per [`Frame`]. `step` itself only
//! dispatches on the shape of [`Focus`]. No arm calls another arm; no arm
//! loops.

use std::sync::Arc;

use crate::io::{self, Sink};
use crate::ir::{Args, CaseArm, Comp, CompKind, GroupNode, Val, ValListElem};
use crate::path::sigil::FreezeCtx;
use crate::runtime::command_call::{self, Resolution};
use crate::runtime::pipeline;
use crate::source::Span;
use crate::syntax::ast::Redirects;
use crate::types::{
    Binding, Break, CapturePolicy, Closure, Env, Error, HandlerArity, HandlerFrame, Mooring,
    Settled, Shell, Signature, TrailScope, Value, as_map, error_record_of, report_value,
};

use super::assemble;
use super::expr;
use super::pattern;
use super::redirect::{RedirectState, WriteFate};
use super::scope::{WithinScope, WithinUndo};
use super::val::{form, form_options, interpolate_piece, spread_type_err};

// ── The stack ─────────────────────────────────────────────────────────

/// What a computation closure settles to, before a frame decides what it
/// means: a value, or a λ awaiting its arguments.
pub(crate) enum Terminal {
    Value(Value),
    /// `λp. M` awaiting an argument, over the environment it was reached in.
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
/// with syntax to close over it: `To` alone. The two large ones, `Redirect`
/// and `Unmask`, are boxed.
enum Frame {
    /// `M to x. N`: `bind` is the `Bind` node itself, read for its pattern,
    /// rest and span.
    To {
        bind: Arc<Comp>,
        env: Env,
    },
    Apply {
        args: Vec<Value>,
        span: Option<Span>,
    },
    Capture {
        prev: Sink,
        buf: io::ByteBuffer,
        span: Option<Span>,
    },
    Redirect(Box<RedirectState>),
    Unmask {
        frame: Box<HandlerFrame>,
    },
    Try {
        handler: Value,
    },
    Guard {
        cleanup: Value,
    },
    Cleanup {
        outcome: Settled<Value>,
    },
    Within(WithinUndo),
    /// Where its layer sits: a session frame pushed above survives the pop.
    Grant(usize),
    Audit {
        scope: TrailScope,
        saved: CapturePolicy,
    },
}

/// `Redirect` and `Unmask` are boxed: any alone would exceed the cap.
const _: () = assert!(std::mem::size_of::<Frame>() <= 128);

/// The state: a focus and a stack. Private — nothing outside this module
/// constructs one or sees the stack.
pub(crate) struct Machine {
    focus: Focus,
    stack: Vec<Frame>,
}

impl Default for Machine {
    /// The one placeholder state: a `Unit` return over the empty stack.
    fn default() -> Self {
        Self {
            focus: Focus::Return(Terminal::Value(Value::Unit)),
            stack: Vec::new(),
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

/// Stamp an unspanned error's span with `span`, the innermost node or frame
/// that raised it — applied at each raising point instead of once at the end
/// of a bigger function.
fn stamp(b: Break, span: Option<Span>) -> Break {
    match (b, span) {
        (Break::Error(e), Some(span)) if e.span.is_none() => Break::Error(e.at_span(span)),
        (b, _) => b,
    }
}

fn stamp_focus(focus: Focus, span: Option<Span>) -> Focus {
    match focus {
        Focus::Halt(b) => Focus::Halt(stamp(b, span)),
        other => other,
    }
}

/// Unreachable for a checked program except where an `F`-holed frame meets a
/// `Lambda` terminal.
fn bare_lambda_error() -> Error {
    Error::new(
        "tried to evaluate a bare lambda in computation position — a function value must be \
         applied to arguments or held as a thunk",
        1,
    )
    .with_hint("either call this function with arguments, or bind it as `Thunk(...)` to pass it as a value")
}

/// `t as Value else Halt(bare lambda)` — the `F`-holed frames' shared move.
fn as_value(t: Terminal) -> Result<Value, Break> {
    match t {
        Terminal::Value(v) => Ok(v),
        Terminal::Lambda { .. } => Err(Break::Error(bare_lambda_error())),
    }
}

/// A capture that cannot become the value it promised: what the body already
/// wrote is an effect that happened, so it goes to the sink the capture
/// replaced before the halt rather than dying in the abandoned buffer.
fn abandon_capture(prev: &mut Sink, bytes: &[u8], error: Error, span: Option<Span>) -> Focus {
    if !bytes.is_empty()
        && let Err(br) = Shell::write_sink(prev, bytes, "the surrounding stream")
    {
        return Focus::Halt(br);
    }
    Focus::Halt(stamp(Break::Error(error), span))
}

fn capture_overflowed() -> Error {
    Error::new(
        format!(
            "capture exceeded {} MiB — the bytes that fit went out to the visible stream rather \
             than into the value, because a prefix is not what the command wrote",
            io::SINK_BUFFER_CAP / (1024 * 1024)
        ),
        1,
    )
    .with_hint("did you mean to write this to a file? `cmd > out.txt` keeps every byte")
}

fn rec_node(group: &Arc<GroupNode>, index: usize) -> Arc<Comp> {
    Arc::new(crate::source::Spanned::synthetic(CompKind::Rec {
        group: Arc::clone(group),
        index,
    }))
}

/// A boundary runs saturated: `annotate` η-expands an under-applied call, so
/// a call that is not is one the checker never saw.
fn run_boundary(
    entry: &crate::types::BuiltinEntry,
    applied: Box<[Value]>,
    argv: Vec<Value>,
    site: Option<&Arc<crate::types::Site>>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let args: Vec<Value> = applied.into_vec().into_iter().chain(argv).collect();
    let arity = entry.fixed_arity();
    if args.len() != arity {
        let name = &entry.name;
        return Err(crate::types::sig(format!(
            "{name}: takes {arity} argument(s) in one call, got {}",
            args.len()
        )));
    }
    super::audit::run_native(entry, &args, site, mooring, shell)
}

pub(crate) fn close_args(args: &Args, env: &Env, sig: &Signature) -> Result<Vec<Value>, Error> {
    let mut out = Vec::with_capacity(args.len());
    for elem in args {
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

impl Machine {
    /// The initial state for `apply`: an empty stack, focus set by `apply_rule`.
    fn applying(f: Value, args: Vec<Value>, mooring: &Mooring, shell: &mut Shell) -> Self {
        let mut m = Self::default();
        m.focus = m.apply_rule(f, args, None, mooring, shell);
        m
    }

    /// The stack cap: refuse a push at `stack.len() >= shell.session.stack_limit`,
    /// before any effect — every pushing rule calls this first.
    fn reserve(&self, shell: &Shell) -> Result<(), Break> {
        if self.stack.len() >= shell.session.stack_limit {
            return Err(Break::Error(
                Error::new(
                    format!(
                        "recursion limit exceeded ({} frames)",
                        shell.session.stack_limit
                    ),
                    1,
                )
                .with_hint(
                    "usually a runaway recursive function — \
                     raise via rc recursion_limit: or --recursion-limit",
                ),
            ));
        }
        Ok(())
    }

    /// The only way a frame gets on the stack; infallible after `reserve`.
    fn push(&mut self, frame: Frame) {
        self.stack.push(frame);
    }

    /// One transition: `step_eval` on `Eval`, `step_return`/`step_halt` otherwise.
    fn step(&mut self, mooring: &Mooring, shell: &mut Shell) {
        let focus = std::mem::replace(&mut self.focus, Self::default().focus);
        self.focus = match focus {
            Focus::Eval { comp, env } => self.step_eval(&comp, env, mooring, shell),
            Focus::Return(t) => {
                let frame = self.stack.pop().expect(
                    "Return with an empty stack: evaluate's loop must stop before calling step again",
                );
                self.step_return(frame, t, mooring, shell)
            }
            Focus::Halt(brk) => {
                let frame = self.stack.pop().expect(
                    "Halt with an empty stack: evaluate's loop must stop before calling step again",
                );
                self.step_halt(frame, brk, mooring, shell)
            }
        };
    }

    // ── force, beta, apply ─────────────────────────────────────────────

    /// `beta(⟨λp. M, E⟩, args)`: a λ meeting its arguments. `args` non-empty.
    fn beta(
        &mut self,
        comp: &Comp,
        env: Env,
        mut args: Vec<Value>,
        span: Option<Span>,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Focus {
        let Some((param, body)) = comp.arrow() else {
            unreachable!("a Terminal::Lambda's closure is always arrow-shaped (eta-expansion)")
        };
        let arg = args.remove(0);
        let env2 = match pattern::bind_pattern(param, &arg, &[], env, shell) {
            Ok(e) => e,
            Err(b) => return Focus::Halt(b),
        };
        if let Err(b) = crate::process::check(mooring) {
            return Focus::Halt(b);
        }
        if !args.is_empty() {
            if let Err(b) = self.reserve(shell) {
                return Focus::Halt(b);
            }
            self.push(Frame::Apply { args, span });
        }
        Focus::Eval {
            comp: Arc::clone(body),
            env: env2,
        }
    }

    /// `apply(f, args)`: a closed value meeting arguments. `args` non-empty.
    fn apply_rule(
        &mut self,
        f: Value,
        mut args: Vec<Value>,
        span: Option<Span>,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Focus {
        debug_assert!(!args.is_empty(), "apply called with empty args");
        match f {
            Value::Thunk(c) => {
                if let Err(b) = self.reserve(shell) {
                    return Focus::Halt(b);
                }
                self.push(Frame::Apply { args, span });
                let (comp, env) = c.into_parts();
                Focus::Eval { comp, env }
            }
            Value::Native { entry, applied } => {
                let needed = entry.fixed_arity();
                let take = needed.saturating_sub(applied.len()).min(args.len());
                let mut collected = applied.into_vec();
                let rest = args.split_off(take);
                collected.extend(args);
                if collected.len() < needed {
                    return Focus::Return(Terminal::Value(Value::Native {
                        entry,
                        applied: collected.into(),
                    }));
                }
                match super::audit::run_native(&entry, &collected, None, mooring, shell) {
                    Ok(v) => {
                        if !rest.is_empty() {
                            if let Err(b) = self.reserve(shell) {
                                return Focus::Halt(b);
                            }
                            self.push(Frame::Apply { args: rest, span });
                        }
                        Focus::Return(Terminal::Value(v))
                    }
                    Err(b) => Focus::Halt(b),
                }
            }
            other => {
                let hint = if matches!(other, Value::Unit) {
                    "too many arguments — the function returned before consuming all of them"
                } else {
                    "only Lambdas, Blocks, and natives are functions"
                };
                Focus::Halt(Break::Error(
                    Error::new(format!("{} is not a function", other.type_name()), 1)
                        .with_hint(hint),
                ))
            }
        }
    }

    /// `force V` on a closed value: `force(thunk M) = M`, nothing pushed.
    fn force(v: Value, mooring: &Mooring, shell: &mut Shell) -> Focus {
        match v {
            Value::Thunk(c) => {
                let (comp, env) = c.into_parts();
                Focus::Eval { comp, env }
            }
            Value::Native { entry, applied } if entry.fixed_arity() == 0 => {
                match super::audit::run_native(&entry, &applied, None, mooring, shell) {
                    Ok(v) => Focus::Return(Terminal::Value(v)),
                    Err(b) => Focus::Halt(b),
                }
            }
            native @ Value::Native { .. } => Focus::Return(Terminal::Value(native)),
            other => Focus::Halt(Break::Error(
                Error::new(
                    format!("cannot force {}: ! requires a Block", other.type_name()),
                    1,
                )
                .with_hint("wrap in a block: !{ expr }"),
            )),
        }
    }

    /// `force V` on a value still syntax: `force(thunk M) = M`, a literal block
    /// running in place and closing nothing.
    fn force_val(
        val: &Val,
        env: Env,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Result<Focus, Break> {
        Ok(match val {
            Val::Thunk(node) => Focus::Eval {
                comp: Arc::clone(node.shape()),
                env,
            },
            other => Self::force(form(other, &env, &shell.sig)?, mooring, shell),
        })
    }

    /// `push_redirect(redirs)`: nothing when empty, else install and push
    /// the `Redirect` frame.
    fn push_redirect(
        &mut self,
        redirs: &Redirects<String>,
        span: Option<Span>,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Result<(), Break> {
        if redirs.is_empty() {
            return Ok(());
        }
        self.reserve(shell)?;
        let state = RedirectState::enter(redirs, span, mooring, shell)?;
        self.push(Frame::Redirect(Box::new(state)));
        Ok(())
    }

    // ── Rules on a computation closure in focus ────────────────────────

    fn step_eval(
        &mut self,
        comp: &Arc<Comp>,
        env: Env,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Focus {
        let focus = self
            .eval_rules(comp, env, mooring, shell)
            .unwrap_or_else(Focus::Halt);
        stamp_focus(focus, comp.span)
    }

    /// One rule per row on [`CompKind`], each raising with `?`. `step_eval`
    /// is their only exit, so no rule can reach the machine without passing
    /// under `stamp_focus`.
    fn eval_rules(
        &mut self,
        comp: &Arc<Comp>,
        env: Env,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Result<Focus, Break> {
        Ok(match &comp.item {
            CompKind::Return(val) => Focus::Return(Terminal::Value(form(val, &env, &shell.sig)?)),

            CompKind::Assemble(assembly) => {
                Focus::Return(Terminal::Value(assemble::eval(assembly, &env, &shell.sig)?))
            }

            // Canonical at A → C, never a value: the frame on top decides.
            // Unreachable except under Apply/a C-holed frame for a checked
            // program.
            CompKind::Lam { .. } => Focus::Return(Terminal::Lambda {
                comp: Arc::clone(comp),
                env,
            }),

            CompKind::Rec { group, index } => {
                crate::process::check(mooring)?;
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
                path.expand(&crate::builtins::ambient::home_dir(shell)?),
            ))),

            CompKind::Force(val) => Self::force_val(val, env, mooring, shell)?,

            CompKind::Interpolation(parts) => {
                let mut s = String::new();
                for p in parts {
                    s.push_str(&interpolate_piece(&form(p, &env, &shell.sig)?)?);
                }
                Focus::Return(Terminal::Value(Value::string(s)))
            }

            CompKind::Binary(op, lhs, rhs) => Focus::Return(Terminal::Value(expr::eval_binary(
                *op, lhs, rhs, &env, &shell.sig,
            )?)),
            CompKind::Negate(v) => {
                Focus::Return(Terminal::Value(expr::eval_negate(v, &env, &shell.sig)?))
            }
            CompKind::Not(v) => {
                Focus::Return(Terminal::Value(expr::eval_not(v, &env, &shell.sig)?))
            }

            CompKind::Index { target, keys } => {
                let mut v = form(target, &env, &shell.sig)?;
                for key in keys {
                    let k = form(&key.item, &env, &shell.sig)?;
                    v = expr::index_value(&v, &k)?;
                }
                Focus::Return(Terminal::Value(v))
            }

            CompKind::Bind {
                comp: rhs,
                pattern: _,
                rest: _,
            } => {
                crate::process::check(mooring)?;
                self.reserve(shell)?;
                self.push(Frame::To {
                    bind: Arc::clone(comp),
                    env: env.clone(),
                });
                Focus::Eval {
                    comp: Arc::clone(rhs),
                    env,
                }
            }

            CompKind::If { cond, then, else_ } => match form(&cond.item, &env, &shell.sig)? {
                Value::Bool(b) => {
                    Self::force_val(&if b { then } else { else_ }.item, env, mooring, shell)?
                }
                other => {
                    return Err(Break::Error(Error::new(
                        format!("if: expected Bool, got {} '{}'", other.type_name(), other),
                        1,
                    )));
                }
            },

            CompKind::Case { scrutinee, arms } => {
                self.step_case(scrutinee, arms, comp.span, env, mooring, shell)?
            }

            CompKind::App { head, args } => {
                crate::process::check(mooring)?;
                shell.stamp_call_site(comp.span);
                let argv = close_args(args, &env, &shell.sig)?;
                self.reserve(shell)?;
                self.push(Frame::Apply {
                    args: argv,
                    span: comp.span,
                });
                Focus::Eval {
                    comp: Arc::clone(head),
                    env,
                }
            }

            CompKind::Exec(exec) => self.step_exec(exec, comp.span, &env, mooring, shell)?,

            CompKind::Pipeline { stages, .. } => {
                if stages.len() == 1 {
                    return Ok(Focus::Eval {
                        comp: Arc::clone(&stages[0]),
                        env,
                    });
                }
                let node = pipeline::PipeNode::launch(stages, &env, mooring, shell)?;
                Focus::Return(Terminal::Value(node.join(mooring, shell)?))
            }

            CompKind::Capture(body) => {
                self.reserve(shell)?;
                let (sink, buf) = io::new_buffer();
                let prev = std::mem::replace(&mut shell.io.stdout, sink);
                self.push(Frame::Capture {
                    prev,
                    buf,
                    span: comp.span,
                });
                Focus::Eval {
                    comp: Arc::clone(body),
                    env,
                }
            }

            CompKind::Decode(val) => {
                let v = form(val, &env, &shell.sig)?;
                // The bind's scope dies before the decode, not after it, so the
                // capture's buffer is unshared and taken over, not copied.
                drop(env);
                let Value::Bytes(bytes) = v else {
                    return Err(Break::Error(
                        Error::new(
                            format!(
                                "the value boundary was handed {} where a captured byte payload belongs",
                                v.type_name()
                            ),
                            1,
                        )
                        .with_hint(
                            "only the type checker writes this step, so this is a fault in ral \
                             rather than in your program — please report the source that produced it",
                        ),
                    ));
                };
                let mut bytes = bytes.into_vec();
                bytes.truncate(bytes.len() - io::terminator_len(&bytes));
                Focus::Return(Terminal::Value(Value::string(
                    crate::builtins::util::decode_utf8_strict(
                        bytes,
                        "captured output is not valid UTF-8 text",
                        "to keep it as bytes, pipe it: `… | from-bytes`",
                    )?,
                )))
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
                let opts = form_options(opts, &env, &shell.sig)?;
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
                let scope = WithinScope::parse(&as_map(&opts, "within")?, arms, &env, shell)?;
                let body = form(body, &env, &shell.sig)?;
                self.reserve(shell)?;
                let undo = scope.enter(shell);
                self.push(Frame::Within(undo));
                Self::force(body, mooring, shell)
            }

            CompKind::Grant { caps, body } => {
                let c = form_options(caps, &env, &shell.sig)?;
                let home = shell.context.home();
                let cwd = shell.cwd();
                let ctx = FreezeCtx {
                    home: home.as_deref(),
                    cwd: &cwd,
                };
                let caps = crate::capability::decode_capability_map(&c, "grant", &ctx)?;
                let body = form(body, &env, &shell.sig)?;
                self.reserve(shell)?;
                let at = shell.context.grants.len();
                shell.context.grants.push(caps);
                shell.audit_deputy_prefixes();
                self.push(Frame::Grant(at));
                Self::force(body, mooring, shell)
            }

            CompKind::Try { body, handler } => {
                let body = form(body, &env, &shell.sig)?;
                let handler = form(handler, &env, &shell.sig)?;
                self.reserve(shell)?;
                self.push(Frame::Try { handler });
                Self::force(body, mooring, shell)
            }

            CompKind::Guard { body, cleanup } => {
                let body = form(body, &env, &shell.sig)?;
                let cleanup = form(cleanup, &env, &shell.sig)?;
                self.reserve(shell)?;
                self.push(Frame::Guard { cleanup });
                Self::force(body, mooring, shell)
            }

            CompKind::Audit { body } => {
                let body = form(body, &env, &shell.sig)?;
                self.reserve(shell)?;
                let saved = shell.local.audit.capture_policy();
                shell
                    .local
                    .audit
                    .set_capture(super::audit::merge_capture(saved, CapturePolicy::Bytes));
                let scope = shell.local.audit.open();
                self.push(Frame::Audit { scope, saved });
                Self::force(body, mooring, shell)
            }
        })
    }

    /// `case`: the chosen arm is forced and applied to the payload.
    fn step_case(
        &mut self,
        scrutinee: &crate::source::Spanned<Val>,
        arms: &[CaseArm],
        span: Option<Span>,
        env: Env,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Result<Focus, Break> {
        let (label, payload) = match form(&scrutinee.item, &env, &shell.sig)? {
            Value::Variant { label, payload } => (label, payload),
            other => {
                return Err(Break::Error(Error::new(
                    format!(
                        "case: scrutinee must be a variant, got {} {}",
                        other.type_name(),
                        other
                    ),
                    1,
                )));
            }
        };
        let Some(arm) = arms.iter().find(|arm| arm.tag.item == *label) else {
            let handled: Vec<String> = arms
                .iter()
                .map(|a| format!("{}{}", crate::syntax::tag::TAG_PREFIX, a.tag.item))
                .collect();
            return Err(Break::Error(Error::new(
                format!(
                    "case: no arm for variant `{label}`; this case matches: {}",
                    handled.join(", ")
                ),
                1,
            )));
        };
        let payload = payload.map_or(Value::Unit, |p| *p);
        self.reserve(shell)?;
        self.push(Frame::Apply {
            args: vec![payload],
            span,
        });
        Self::force_val(&arm.body.item, env, mooring, shell)
    }

    fn step_exec(
        &mut self,
        exec: &crate::ir::Exec,
        span: Option<Span>,
        env: &Env,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Result<Focus, Break> {
        crate::process::check(mooring)?;
        let argv = close_args(&exec.args, env, &shell.sig)?;
        let redirs = close_redirects(&exec.redirects, env, &shell.sig)?;
        shell.stamp_call_site(span);
        Ok(
            match command_call::classify_command(&exec.head, env, mooring, shell)? {
                Resolution::Env(Value::Native { entry, applied }) if entry.is_boundary() => {
                    self.push_redirect(&redirs, span, mooring, shell)?;
                    let site = exec.site.as_ref();
                    Focus::Return(Terminal::Value(run_boundary(
                        &entry, applied, argv, site, mooring, shell,
                    )?))
                }
                Resolution::Env(v) => {
                    self.push_redirect(&redirs, span, mooring, shell)?;
                    if argv.is_empty() {
                        Self::force(v, mooring, shell)
                    } else {
                        self.apply_rule(v, argv, span, mooring, shell)
                    }
                }
                Resolution::Handler { entry, depth } => {
                    self.push_redirect(&redirs, span, mooring, shell)?;
                    self.reserve(shell)?;
                    let frame = Box::new(shell.context.handlers.strip_matched(depth));
                    self.push(Frame::Unmask { frame });
                    let call_args = render_handler_args(&entry.name, entry.arity, &argv);
                    self.apply_rule(entry.thunk.clone(), call_args, span, mooring, shell)
                }
                Resolution::Base(entry) => Focus::Return(Terminal::Value(
                    command_call::run_base_frame(&entry, &argv, &redirs, span, mooring, shell)?,
                )),
                Resolution::External(head) => Focus::Return(Terminal::Value(
                    command_call::run_external(&head, &argv, &redirs, span, mooring, shell)?,
                )),
            },
        )
    }

    // ── Frames, and their two rules each ───────────────────────────────

    fn step_return(
        &mut self,
        frame: Frame,
        t: Terminal,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Focus {
        match frame {
            Frame::To { bind, env } => {
                let v = match as_value(t) {
                    Ok(v) => v,
                    Err(b) => return Focus::Halt(b),
                };
                let CompKind::Bind { pattern, rest, .. } = &bind.item else {
                    unreachable!("a To frame's `bind` is always a Bind comp")
                };
                match pattern::bind_pattern(pattern, &v, &[], env, shell) {
                    Ok(env2) => Focus::Eval {
                        comp: Arc::clone(rest),
                        env: env2,
                    },
                    Err(b) => Focus::Halt(stamp(b, bind.span)),
                }
            }

            Frame::Apply { args, span } => match t {
                Terminal::Lambda { comp, env } => {
                    stamp_focus(self.beta(&comp, env, args, span, mooring, shell), span)
                }
                Terminal::Value(v) => {
                    stamp_focus(self.apply_rule(v, args, span, mooring, shell), span)
                }
            },

            Frame::Capture {
                mut prev,
                buf,
                span,
            } => {
                let bytes = io::take_buffer(&buf);
                let overflowed = io::buffer_overflowed(&buf);
                let focus = if overflowed {
                    abandon_capture(&mut prev, &bytes, capture_overflowed(), span)
                } else {
                    Focus::Return(Terminal::Value(Value::bytes(bytes)))
                };
                shell.io.stdout = prev;
                focus
            }

            Frame::Redirect(state) => {
                let mut state = *state;
                match state.leave(WriteFate::Commit, mooring, shell) {
                    Ok(()) => Focus::Return(t),
                    Err(b) => Focus::Halt(b),
                }
            }

            Frame::Unmask { frame } => {
                shell.context.handlers.restore_matched(*frame);
                Focus::Return(t)
            }

            Frame::Try { .. } => Focus::Return(t),

            Frame::Guard { cleanup } => {
                let v = match as_value(t) {
                    Ok(v) => v,
                    Err(b) => return Focus::Halt(b),
                };
                if let Err(b) = self.reserve(shell) {
                    return Focus::Halt(b);
                }
                self.push(Frame::Cleanup { outcome: Ok(v) });
                Self::force(cleanup, mooring, shell)
            }

            Frame::Cleanup { outcome } => match outcome {
                Ok(v) => Focus::Return(Terminal::Value(v)),
                Err(s) => Focus::Halt(s),
            },

            Frame::Within(undo) => {
                undo.apply(shell);
                Focus::Return(t)
            }

            Frame::Grant(at) => {
                shell.context.grants.remove(at, 1);
                Focus::Return(t)
            }

            Frame::Audit { scope, saved } => {
                let trail = shell.local.audit.close(scope);
                shell.local.audit.set_capture(saved);
                let v = match as_value(t) {
                    Ok(v) => v,
                    Err(b) => return Focus::Halt(b),
                };
                Focus::Return(Terminal::Value(report_value(Ok(v), &trail)))
            }
        }
    }

    fn step_halt(&mut self, frame: Frame, s: Break, mooring: &Mooring, shell: &mut Shell) -> Focus {
        match frame {
            Frame::To { .. } => Focus::Halt(match s {
                Break::Error(e) if e.hint.is_none() => {
                    Break::Error(e.with_hint(super::ABANDONED_TAIL_HINT))
                }
                s => s,
            }),

            Frame::Capture { mut prev, buf, .. } => {
                let bytes = io::take_buffer(&buf);
                let flushed = Shell::write_sink(&mut prev, &bytes, "the surrounding stream");
                shell.io.stdout = prev;
                Focus::Halt(flushed.err().unwrap_or(s))
            }

            Frame::Apply { .. } | Frame::Cleanup { .. } => Focus::Halt(s),

            Frame::Redirect(state) => {
                let mut state = *state;
                let _ = state.leave(WriteFate::Abort, mooring, shell);
                Focus::Halt(s)
            }

            Frame::Unmask { frame } => {
                shell.context.handlers.restore_matched(*frame);
                Focus::Halt(s)
            }

            Frame::Try { handler } => match s {
                Break::Error(e) => {
                    let record = error_record_of(&e, shell);
                    self.apply_rule(handler, vec![record], None, mooring, shell)
                }
                Break::Escape(esc) => Focus::Halt(Break::Escape(esc)),
            },

            Frame::Guard { cleanup } => {
                if let Err(b) = self.reserve(shell) {
                    return Focus::Halt(b);
                }
                self.push(Frame::Cleanup { outcome: Err(s) });
                Self::force(cleanup, mooring, shell)
            }

            Frame::Within(undo) => {
                undo.apply(shell);
                Focus::Halt(s)
            }

            Frame::Grant(at) => {
                shell.context.grants.remove(at, 1);
                Focus::Halt(s)
            }

            Frame::Audit { scope, saved } => match s {
                Break::Error(e) => {
                    let trail = shell.local.audit.close(scope);
                    shell.local.audit.set_capture(saved);
                    let record = error_record_of(&e, shell);
                    Focus::Return(Terminal::Value(report_value(Err(record), &trail)))
                }
                Break::Escape(esc) => {
                    let _trail = shell.local.audit.close(scope);
                    shell.local.audit.set_capture(saved);
                    Focus::Halt(Break::Escape(esc))
                }
            },
        }
    }
}

impl Frame {
    /// The panic path: undo what a checkpoint cannot hold — fds, staging
    /// files, audit scopes. `To`/`Capture` restore `io.stdout`;
    /// `Redirect` as its own rule; `Unmask` restores; `Audit`
    /// `audit.close(scope)` then `set_capture(saved)`, discarding the trail
    /// no one is left to read; `Within` applies its undo; `Grant` removes its layer.
    /// `Apply`, `Try`, `Guard`, `Cleanup` do nothing.
    fn abandon(self, shell: &mut Shell) {
        match self {
            Self::Capture { prev, .. } => shell.io.stdout = prev,
            Self::Redirect(state) => state.abandon(shell),
            Self::Unmask { frame } => shell.context.handlers.restore_matched(*frame),
            Self::Audit { scope, saved } => {
                let _trail = shell.local.audit.close(scope);
                shell.local.audit.set_capture(saved);
            }
            Self::Within(undo) => undo.apply(shell),
            Self::Grant(at) => shell.context.grants.remove(at, 1),
            Self::To { .. }
            | Self::Apply { .. }
            | Self::Try { .. }
            | Self::Guard { .. }
            | Self::Cleanup { .. } => {}
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
        return Err(Break::Error(
            Error::new(
                format!("native re-entry depth exceeded ({NESTED_MACHINE_LIMIT})"),
                1,
            )
            .with_hint(
                "a builtin such as `map` applied a function that itself applied `map`, too deep — \
                 restructure with a tail-recursive loop",
            ),
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
            while let Some(frame) = m.stack.pop() {
                frame.abandon(shell);
            }
            std::panic::resume_unwind(payload);
        }
    }
}

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

/// The same, from a closed value meeting arguments — `Machine::applying`
/// sets the first state.
pub(crate) fn apply(
    f: Value,
    args: Vec<Value>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    run(
        |m, mooring, shell| *m = Machine::applying(f, args, mooring, shell),
        mooring,
        shell,
    )
}

/// `force v` on a value already in hand (a door — the hook table's arity-0
/// entries, a worker's body — rather than a `CompKind::Force` node):
/// `Machine::force`'s rule, run as its own machine.
pub(crate) fn force(v: Value, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    run(
        |m, mooring, shell| m.focus = Machine::force(v, mooring, shell),
        mooring,
        shell,
    )
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluator::{run_source, with_capture};
    use crate::ir::{Name, Phrase};
    use crate::source::{FileId, Spanned};
    use crate::types::{Break, captured};

    /// Run every phrase of `source` through the machine: a `Define` binds
    /// into the running `Env` with `pattern::bind_pattern`, a `Run` phrase's
    /// value is the previous one, mirroring `run_phrases`'s shape without
    /// its lease machinery, irrelevant to what these tests probe.
    fn run_with(source: &str, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
        let top = crate::compile_and_typecheck(
            source,
            crate::typecheck::SessionSchemes::default(),
            FileId::DUMMY,
            "<test>",
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
                    env = pattern::bind_pattern(pattern, &v, &[], env, shell)?;
                    value = Ok(Value::Unit);
                }
            }
        }
        shell.env = env;
        value
    }

    fn new_shell() -> Shell {
        Shell::new(crate::io::TerminalState::default())
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

    /// A capture that fails flushes what it wrote to the sink it replaced,
    /// then fails.
    #[test]
    fn a_failing_capture_flushes_what_it_wrote_before_it_fails() {
        let mut shell = new_shell();
        let (result, bytes, _overflowed) = with_capture(&mut shell, |shell| {
            run_source(
                "alias boom { |a| echo partial; fail [status: 1, message: 'x'] }\n\
                 let x = boom\nreturn $x",
                shell,
            )
        });
        assert!(result.is_err(), "the stand-in fails");
        assert_eq!(bytes, b"partial\n");
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
            Err(Break::Escape(crate::types::Escape::Exit(5))) => {}
            other => panic!("expected Escape::Exit(5) to propagate past the chain, got {other:?}"),
        }
    }

    /// Over-application raises the "not a function" text, stamped with the
    /// call's own span. Built by hand rather than parsed: `App`ing a value
    /// twice is rejected statically by the checker (its type never unifies
    /// with a further arrow), so the runtime rule is unreachable from real
    /// source text — this is exactly why it is a rule at all (unreachable
    /// for a *checked* program), and the machine must still have it.
    #[test]
    fn over_application_is_stamped_with_the_calls_span() {
        let mut shell = new_shell();
        let file = FileId::DUMMY;
        let span = crate::source::Span::new(file, 10, 20);
        let head = Arc::new(Spanned::with_span(
            Some(span),
            CompKind::Return(Val::Int(5)),
        ));
        let args: Args = vec![ValListElem::Single(Spanned::with_span(
            Some(span),
            Val::Int(1),
        ))];
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
                    e.message.contains("is not a function"),
                    "got {:?}",
                    e.message
                );
                assert_eq!(
                    e.span,
                    Some(span),
                    "the over-application error must carry the call's span"
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
                    e.message.contains("recursion limit exceeded"),
                    "got {:?}",
                    e.message
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

    /// A cancelled `let f = { !f }; !f` returns rather than spinning: `Rec`
    /// is the one polled rule such a loop reaches.
    #[test]
    fn a_cancelled_tight_loop_returns() {
        let mut shell = new_shell();
        let mooring = Mooring::adrift();
        mooring
            .cancel
            .cancel(crate::process::cancel::CancelCause::Explicit);
        let out = run_with("let f = { !$f }\n!$f", &mooring, &mut shell);
        assert!(
            out.is_err(),
            "a cancelled tight loop must halt rather than spin"
        );
    }

    /// A cancellation is a `Break` like any other: whichever polled rule
    /// sees it, the halt leaves `step_eval` through `stamp_focus` and so
    /// carries that node's span.
    #[test]
    fn a_cancelled_rec_and_a_cancelled_bind_both_carry_a_span() {
        for source in ["let f = { !$f }\n!$f", "if true { let x = 1; return $x }"] {
            let mut shell = new_shell();
            let mooring = Mooring::adrift();
            mooring
                .cancel
                .cancel(crate::process::cancel::CancelCause::Explicit);
            match run_with(source, &mooring, &mut shell) {
                Err(Break::Error(e)) => assert!(
                    e.span.is_some(),
                    "a cancellation in `{source}` must be located, got {:?}",
                    e.message
                ),
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
                        e.message.contains("native re-entry depth exceeded"),
                        "expected the depth-exceeded error, got {:?}",
                        e.message
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
