//! The machine: closures in focus, frames on a stack, one `step` (§2 of the
//! CEK plan, `dev/docs/plans/260825_cek_machine.md`).
//!
//! [`Machine::step_eval`] is §2.2, one match arm per row on `CompKind`;
//! [`Machine::step_return`] and [`Machine::step_halt`] are §2.3's two
//! columns, one match arm per row on [`Frame`]. `step` itself only
//! dispatches on the shape of [`Focus`]. No arm calls another arm; no arm
//! loops.

use std::sync::Arc;

use crate::ir::{
    Args, CaseArm, Comp, CompKind, RedirectV, Val, ValListElem, ValRedirectTarget,
};
use crate::io::{self, Sink};
use crate::path::sigil::FreezeCtx;
use crate::runtime::command::{self, EvalRedirect, EvalRedirectV};
use crate::runtime::command_call::{self, Resolution};
use crate::runtime::pipeline;
use crate::source::Span;
use crate::types::{
    Binding, Break, CapturePolicy, Closure, Env, Error, HandlerArity, HandlerFrame, Mooring,
    Settled, Shell, TrailScope, Value, as_map, tree_value,
};

use super::pattern;
use super::redirect::{RedirectState, WriteFate};
use super::scope::{WithinScope, WithinUndo, classify, error_record};
use super::val::{close, interpolate_piece, spread_type_err};
use super::{expr, observe};

// ── §1.4 / §2.3: the stack ─────────────────────────────────────────────

/// What a computation closure settles to, before a frame decides what it
/// means: a value, or a λ awaiting its arguments.
pub(crate) enum Terminal {
    Value(Value),
    Lambda(Closure),
}

/// The machine's one register.
pub(crate) enum Focus {
    /// A computation closure to step.
    Eval(Closure),
    /// A terminal meeting the frame above.
    Return(Terminal),
    /// A signal climbing the stack.
    Halt(Break),
}

/// One elimination form with a pending sub-computation. Frames hold `Arc`s
/// into the IR and an `Env`, never cloned IR, never a `Context` clone. The
/// two large ones, `Redirect` and (W3) `Pipe`, are boxed.
enum Frame {
    /// `M to x. N`: `bind` is the `Bind` node itself, read for its pattern,
    /// rest and span.
    To {
        bind: Arc<Comp>,
        env: Env,
        prev_stdout: Sink,
    },
    /// `a ? b ? c`: `node` is the `Chain` node, read for its parts.
    Chain {
        node: Arc<Comp>,
        next: usize,
        env: Env,
    },
    Apply {
        args: Vec<Value>,
        env: Env,
        span: Option<Span>,
    },
    Capture {
        prev: Sink,
        buf: io::ByteBuffer,
        span: Option<Span>,
    },
    Decode {
        span: Option<Span>,
    },
    Source {
        rest: Arc<Comp>,
        env: Env,
        span: Option<Span>,
    },
    Redirect(Box<RedirectState>),
    Unmask {
        frame: Box<HandlerFrame>,
    },
    Try {
        handler: Value,
        env: Box<Env>,
        scope: TrailScope,
        saved: CapturePolicy,
    },
    Guard {
        cleanup: Value,
        env: Box<Env>,
    },
    Cleanup {
        outcome: Settled<Value>,
    },
    Within(WithinUndo),
    Grant,
    Audit {
        scope: TrailScope,
        saved: CapturePolicy,
    },
}

/// `Redirect` and `Unmask` are boxed: either alone would exceed the cap.
const _: () = assert!(std::mem::size_of::<Frame>() <= 128);

/// The state: a focus and a stack. Private — nothing outside this module
/// constructs one or sees the stack.
pub(crate) struct Machine {
    focus: Focus,
    stack: Vec<Frame>,
}

/// Nested `machine::evaluate`/`machine::apply` re-entry cap (§2.1, §4): a
/// native such as `map` applying a user function runs a machine over the
/// *host* stack, so this bounds how deep those nest rather than the
/// machine's own frame count. Calibrated by `nested_machines_fit_a_worker_stack`
/// against a debug build's per-frame cost on a 2 MiB worker stack — 256 and
/// 64 both overflowed before tripping the check; 24 leaves headroom.
pub(crate) const NESTED_MACHINE_LIMIT: usize = 24;

// ── Small pure helpers, named in §2.2/§2.3 ─────────────────────────────

/// A Bool result becomes `last_status` (true → 0); any other value leaves it
/// untouched.
pub(crate) fn set_status(v: &Value, shell: &mut Shell) {
    if let Value::Bool(b) = v {
        shell.set_status_from_bool(*b);
    }
}

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

/// The text of `comp.rs:41–48`, unreachable for a checked program except
/// where an `F`-holed frame meets a `Lambda` terminal.
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
        Terminal::Lambda(_) => Err(Break::Error(bare_lambda_error())),
    }
}

fn rec_node(group: &Arc<[(String, Arc<Comp>)]>, index: usize) -> Arc<Comp> {
    Arc::new(crate::source::Spanned::synthetic(CompKind::Rec {
        group: group.clone(),
        index,
    }))
}

pub(crate) fn close_args(args: &Args, env: &Env) -> Result<Vec<Value>, Error> {
    let mut out = Vec::with_capacity(args.len());
    for elem in args {
        match &elem.item {
            ValListElem::Single(v) => out.push(close(v, env)?),
            ValListElem::Spread(v) => match close(v, env)? {
                Value::List(list) => out.extend(list),
                other => return Err(spread_type_err(&other)),
            },
        }
    }
    Ok(out)
}

pub(crate) fn close_redirects(redirects: &[RedirectV], env: &Env) -> Result<Vec<EvalRedirectV>, Error> {
    redirects
        .iter()
        .map(|r| {
            Ok(EvalRedirectV {
                fd: r.fd,
                mode: r.mode,
                target: match &r.target {
                    ValRedirectTarget::File(v) => EvalRedirect::File(close(v, env)?.to_string()),
                    ValRedirectTarget::Fd(n) => EvalRedirect::Fd(*n),
                },
            })
        })
        .collect()
}

fn render_handler_args(name: &str, arity: HandlerArity, argv: &[Value]) -> Vec<Value> {
    let list = Value::list(
        Value::render_argv(argv)
            .into_iter()
            .map(Value::String)
            .collect(),
    );
    match arity {
        HandlerArity::CatchAll => vec![Value::String(name.to_string()), list],
        HandlerArity::Unary => vec![list],
    }
}

impl Machine {
    /// The initial state: ⟨M, E⟩ over the empty stack.
    fn inject(closure: Closure) -> Self {
        Self {
            focus: Focus::Eval(closure),
            stack: Vec::new(),
        }
    }

    /// The initial state for `apply`: an empty stack, focus set by `apply_rule` (§2.4).
    fn applying(f: Value, args: Vec<Value>, env: Env, mooring: &Mooring, shell: &mut Shell) -> Self {
        let mut m = Self {
            focus: Focus::Return(Terminal::Value(Value::Unit)),
            stack: Vec::new(),
        };
        m.focus = m.apply_rule(f, args, env, None, mooring, shell);
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

    /// One transition: §2.2 on `Eval`, §2.3 on `Return`/`Halt`.
    fn step(&mut self, mooring: &Mooring, shell: &mut Shell) {
        let focus = std::mem::replace(&mut self.focus, Focus::Return(Terminal::Value(Value::Unit)));
        self.focus = match focus {
            Focus::Eval(closure) => self.step_eval(closure, mooring, shell),
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

    // ── §2.4: force, beta, apply ────────────────────────────────────────

    /// `beta(⟨λp. M, E⟩, args)`: a λ meeting its arguments. `args` non-empty.
    fn beta(
        &mut self,
        lam: Closure,
        mut args: Vec<Value>,
        frame_env: Env,
        span: Option<Span>,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Focus {
        let Closure { comp, env } = lam;
        let Some((param, body)) = comp.arrow() else {
            unreachable!("a Terminal::Lambda's closure is always arrow-shaped (S3 eta-expansion)")
        };
        let arg = args.remove(0);
        let env2 = match pattern::bind_pattern(param, &arg, &[], env, mooring, shell) {
            Ok(e) => e,
            Err(b) => return Focus::Halt(b),
        };
        shell.last_status = 0;
        if let Err(b) = crate::process::check(mooring) {
            return Focus::Halt(b);
        }
        if !args.is_empty() {
            if let Err(b) = self.reserve(shell) {
                return Focus::Halt(b);
            }
            self.push(Frame::Apply {
                args,
                env: frame_env,
                span,
            });
        }
        Focus::Eval(Closure { comp: Arc::clone(body), env: env2 })
    }

    /// `apply(f, args, &env)`: a closed value meeting arguments under the
    /// lexical environment of the call. `args` non-empty.
    fn apply_rule(
        &mut self,
        f: Value,
        mut args: Vec<Value>,
        env: Env,
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
                self.push(Frame::Apply { args, env, span });
                Focus::Eval(c)
            }
            Value::Native { entry, applied } => {
                let needed = entry.fixed_arity();
                let take = needed.saturating_sub(applied.len()).min(args.len());
                let mut collected = applied;
                let rest = args.split_off(take);
                collected.extend(args);
                if collected.len() < needed {
                    return Focus::Return(Terminal::Value(Value::Native { entry, applied: collected }));
                }
                match super::audit::run_native(&entry, &collected, &env, mooring, shell) {
                    Ok(v) => {
                        if !rest.is_empty() {
                            if let Err(b) = self.reserve(shell) {
                                return Focus::Halt(b);
                            }
                            self.push(Frame::Apply { args: rest, env, span });
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
                    Error::new(format!("{} is not a function", other.type_name()), 1).with_hint(hint),
                ))
            }
        }
    }

    /// `force V` on a closed value: `force(thunk M) = M`, nothing pushed (S10).
    fn force(&self, v: Value, env: &Env, mooring: &Mooring, shell: &mut Shell) -> Focus {
        match v {
            Value::Thunk(c) => Focus::Eval(c),
            Value::Native { entry, applied } if entry.fixed_arity() == 0 => {
                match super::audit::run_native(&entry, &applied, env, mooring, shell) {
                    Ok(v) => {
                        set_status(&v, shell);
                        Focus::Return(Terminal::Value(v))
                    }
                    Err(b) => Focus::Halt(b),
                }
            }
            native @ Value::Native { .. } => Focus::Return(Terminal::Value(native)),
            other => Focus::Halt(Break::Error(shell.err_hint(
                format!("cannot force {}: ! requires a Block", other.type_name()),
                "wrap in a block: !{ expr }",
                1,
            ))),
        }
    }

    /// `push_redirect(redirs)`: nothing when empty, else install and push
    /// the `Redirect` frame (§2.2, last paragraph before §2.3).
    fn push_redirect(
        &mut self,
        redirs: Vec<EvalRedirectV>,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Result<(), Break> {
        if redirs.is_empty() {
            return Ok(());
        }
        self.reserve(shell)?;
        let state = RedirectState::enter(&redirs, mooring, shell)?;
        self.push(Frame::Redirect(Box::new(state)));
        Ok(())
    }

    // ── §2.2: rules on a computation closure in focus ───────────────────

    fn step_eval(&mut self, closure: Closure, mooring: &Mooring, shell: &mut Shell) -> Focus {
        let Closure { comp, env } = closure;
        let focus = match &comp.item {
            CompKind::Return(val) => match close(val, &env) {
                Ok(v) => {
                    set_status(&v, shell);
                    Focus::Return(Terminal::Value(v))
                }
                Err(e) => Focus::Halt(Break::Error(e)),
            },

            // Canonical at A → C, never a value: the frame on top decides.
            // Unreachable except under Apply/a C-holed frame for a checked
            // program (§3.5, S3).
            CompKind::Lam { .. } => Focus::Return(Terminal::Lambda(Closure {
                comp: Arc::clone(&comp),
                env,
            })),

            CompKind::Rec { group, index } => {
                if let Err(b) = crate::process::check(mooring) {
                    return Focus::Halt(b);
                }
                let mut env2 = env.clone();
                for (j, (name, _)) in group.iter().enumerate() {
                    env2.bind(
                        name.clone(),
                        Binding {
                            value: Value::Thunk(Closure {
                                comp: rec_node(group, j),
                                env: env.clone(),
                            }),
                            scheme: None,
                        },
                    );
                }
                Focus::Eval(Closure {
                    comp: Arc::clone(&group[*index].1),
                    env: env2,
                })
            }

            CompKind::Observe(reg) => match observe::observe(reg, shell) {
                Ok(v) => Focus::Return(Terminal::Value(v)),
                Err(e) => Focus::Halt(Break::Error(e)),
            },

            CompKind::Force(val) => match close(val, &env) {
                Ok(v) => self.force(v, &env, mooring, shell),
                Err(e) => Focus::Halt(Break::Error(e)),
            },

            CompKind::Interpolation(parts) => 'arm: {
                let mut s = String::new();
                for p in parts {
                    let v = match close(p, &env) {
                        Ok(v) => v,
                        Err(e) => break 'arm Focus::Halt(Break::Error(e)),
                    };
                    match interpolate_piece(&v) {
                        Ok(piece) => s.push_str(&piece),
                        Err(e) => break 'arm Focus::Halt(Break::Error(e)),
                    }
                }
                Focus::Return(Terminal::Value(Value::String(s)))
            }

            CompKind::Binary(op, lhs, rhs) => match expr::eval_binary(*op, lhs, rhs, &env) {
                Ok(v) => Focus::Return(Terminal::Value(v)),
                Err(b) => Focus::Halt(b),
            },
            CompKind::Negate(v) => match expr::eval_negate(v, &env) {
                Ok(val) => Focus::Return(Terminal::Value(val)),
                Err(e) => Focus::Halt(Break::Error(e)),
            },
            CompKind::Not(v) => match expr::eval_not(v, &env) {
                Ok(val) => Focus::Return(Terminal::Value(val)),
                Err(e) => Focus::Halt(Break::Error(e)),
            },

            CompKind::Index { target, keys } => 'arm: {
                let mut v = match close(target, &env) {
                    Ok(v) => v,
                    Err(e) => break 'arm Focus::Halt(Break::Error(e)),
                };
                for key in keys {
                    let k = match close(&key.item, &env) {
                        Ok(k) => k,
                        Err(e) => break 'arm Focus::Halt(Break::Error(e)),
                    };
                    v = match expr::index_value(&v, &k) {
                        Ok(v) => v,
                        Err(e) => break 'arm Focus::Halt(Break::Error(e)),
                    };
                }
                Focus::Return(Terminal::Value(v))
            }

            CompKind::Bind { comp: rhs, pattern: _, rest: _ } => 'arm: {
                if let Err(b) = crate::process::check(mooring) {
                    break 'arm Focus::Halt(b);
                }
                if let Err(b) = self.reserve(shell) {
                    break 'arm Focus::Halt(b);
                }
                let prev = shell.io.to_ambient();
                self.push(Frame::To {
                    bind: Arc::clone(&comp),
                    env: env.clone(),
                    prev_stdout: prev,
                });
                Focus::Eval(Closure {
                    comp: Arc::clone(rhs),
                    env,
                })
            }

            CompKind::Chain(parts) => {
                if parts.is_empty() {
                    Focus::Return(Terminal::Value(Value::Unit))
                } else if let Err(b) = self.reserve(shell) {
                    Focus::Halt(b)
                } else {
                    self.push(Frame::Chain {
                        node: Arc::clone(&comp),
                        next: 1,
                        env: env.clone(),
                    });
                    Focus::Eval(Closure {
                        comp: Arc::clone(&parts[0]),
                        env,
                    })
                }
            }

            CompKind::If { cond, then, else_ } => match close(&cond.item, &env) {
                Ok(Value::Bool(b)) => {
                    shell.set_status_from_bool(b);
                    let branch = if b { then } else { else_ };
                    Focus::Eval(Closure {
                        comp: Arc::clone(branch),
                        env,
                    })
                }
                Ok(other) => Focus::Halt(Break::Error(
                    shell.err(
                        format!("if: expected Bool, got {} '{}'", other.type_name(), other),
                        1,
                    ),
                )),
                Err(e) => Focus::Halt(Break::Error(e)),
            },

            CompKind::Case { scrutinee, arms } => self.step_case(scrutinee, arms, &env, mooring, shell),

            CompKind::App { head, args } => 'arm: {
                if let Err(b) = crate::process::check(mooring) {
                    break 'arm Focus::Halt(b);
                }
                let argv = match close_args(args, &env) {
                    Ok(v) => v,
                    Err(e) => break 'arm Focus::Halt(Break::Error(e)),
                };
                if let Err(b) = self.reserve(shell) {
                    break 'arm Focus::Halt(b);
                }
                self.push(Frame::Apply {
                    args: argv,
                    env: env.clone(),
                    span: comp.span,
                });
                Focus::Eval(Closure {
                    comp: Arc::clone(head),
                    env,
                })
            }

            CompKind::Exec(exec) => self.step_exec(exec, comp.span, &env, mooring, shell),

            CompKind::Pipeline { stages, yields, .. } => {
                if stages.len() == 1 {
                    Focus::Eval(Closure {
                        comp: Arc::clone(&stages[0]),
                        env,
                    })
                } else {
                    match pipeline::run_pipeline(stages, *yields, &env, mooring, shell) {
                        Ok(v) => Focus::Return(Terminal::Value(v)),
                        Err(b) => Focus::Halt(b),
                    }
                }
            }

            CompKind::Capture(body) => 'arm: {
                if let Err(b) = self.reserve(shell) {
                    break 'arm Focus::Halt(b);
                }
                let (sink, buf) = io::new_buffer();
                let prev = std::mem::replace(&mut shell.io.stdout, sink);
                self.push(Frame::Capture {
                    prev,
                    buf,
                    span: comp.span,
                });
                Focus::Eval(Closure {
                    comp: Arc::clone(body),
                    env,
                })
            }

            CompKind::Decode(body) => 'arm: {
                if let Err(b) = self.reserve(shell) {
                    break 'arm Focus::Halt(b);
                }
                self.push(Frame::Decode { span: comp.span });
                Focus::Eval(Closure {
                    comp: Arc::clone(body),
                    env,
                })
            }

            CompKind::Source { path, rest } => 'arm: {
                if let Err(b) = crate::process::check(mooring) {
                    break 'arm Focus::Halt(b);
                }
                if let Err(b) = self.reserve(shell) {
                    break 'arm Focus::Halt(b);
                }
                self.push(Frame::Source {
                    rest: Arc::clone(rest),
                    env: env.clone(),
                    span: comp.span,
                });
                Focus::Eval(Closure {
                    comp: Arc::clone(path),
                    env,
                })
            }

            CompKind::Redirect { body, redirects } => 'arm: {
                let redirs = match close_redirects(redirects, &env) {
                    Ok(r) => r,
                    Err(e) => break 'arm Focus::Halt(Break::Error(e)),
                };
                if let Err(b) = self.push_redirect(redirs, mooring, shell) {
                    break 'arm Focus::Halt(b);
                }
                Focus::Eval(Closure {
                    comp: Arc::clone(body),
                    env,
                })
            }

            CompKind::Within { opts, body } => 'arm: {
                let o = match close(opts, &env) {
                    Ok(v) => v,
                    Err(e) => break 'arm Focus::Halt(Break::Error(e)),
                };
                let m = match as_map(&o, "within") {
                    Ok(m) => m,
                    Err(e) => break 'arm Focus::Halt(Break::Error(e)),
                };
                let scope = match WithinScope::parse(&m, &env, shell) {
                    Ok(s) => s,
                    Err(b) => break 'arm Focus::Halt(b),
                };
                let b = match close(body, &env) {
                    Ok(v) => v,
                    Err(e) => break 'arm Focus::Halt(Break::Error(e)),
                };
                if let Err(err) = self.reserve(shell) {
                    break 'arm Focus::Halt(err);
                }
                let undo = scope.enter(shell);
                self.push(Frame::Within(undo));
                self.force(b, &env, mooring, shell)
            }

            CompKind::Grant { caps, body } => 'arm: {
                let c = match close(caps, &env) {
                    Ok(v) => v,
                    Err(e) => break 'arm Focus::Halt(Break::Error(e)),
                };
                let home = shell.context.home();
                let cwd = shell.cwd();
                let ctx = FreezeCtx {
                    home: home.as_deref(),
                    cwd: &cwd,
                };
                let caps = match crate::capability::decode_capability_map(&c, "grant", &ctx) {
                    Ok(c) => c,
                    Err(e) => break 'arm Focus::Halt(Break::from(e)),
                };
                let b = match close(body, &env) {
                    Ok(v) => v,
                    Err(e) => break 'arm Focus::Halt(Break::Error(e)),
                };
                if let Err(err) = self.reserve(shell) {
                    break 'arm Focus::Halt(err);
                }
                shell.context.grants.push(caps);
                shell.audit_deputy_prefixes();
                self.push(Frame::Grant);
                self.force(b, &env, mooring, shell)
            }

            CompKind::Try { body, handler } => 'arm: {
                let b = match close(body, &env) {
                    Ok(v) => v,
                    Err(e) => break 'arm Focus::Halt(Break::Error(e)),
                };
                let h = match close(handler, &env) {
                    Ok(v) => v,
                    Err(e) => break 'arm Focus::Halt(Break::Error(e)),
                };
                if let Err(err) = self.reserve(shell) {
                    break 'arm Focus::Halt(err);
                }
                let saved = shell.local.audit.capture_policy();
                shell
                    .local
                    .audit
                    .set_capture(super::audit::merge_capture(saved, CapturePolicy::Off));
                let scope = shell.local.audit.open();
                self.push(Frame::Try { handler: h, env: Box::new(env.clone()), scope, saved });
                self.force(b, &env, mooring, shell)
            }

            CompKind::Guard { body, cleanup } => 'arm: {
                let b = match close(body, &env) {
                    Ok(v) => v,
                    Err(e) => break 'arm Focus::Halt(Break::Error(e)),
                };
                let c = match close(cleanup, &env) {
                    Ok(v) => v,
                    Err(e) => break 'arm Focus::Halt(Break::Error(e)),
                };
                if let Err(err) = self.reserve(shell) {
                    break 'arm Focus::Halt(err);
                }
                self.push(Frame::Guard { cleanup: c, env: Box::new(env.clone()) });
                self.force(b, &env, mooring, shell)
            }

            CompKind::Audit { body } => 'arm: {
                let b = match close(body, &env) {
                    Ok(v) => v,
                    Err(e) => break 'arm Focus::Halt(Break::Error(e)),
                };
                if let Err(err) = self.reserve(shell) {
                    break 'arm Focus::Halt(err);
                }
                let saved = shell.local.audit.capture_policy();
                shell
                    .local
                    .audit
                    .set_capture(super::audit::merge_capture(saved, CapturePolicy::Bytes));
                let scope = shell.local.audit.open();
                self.push(Frame::Audit { scope, saved });
                self.force(b, &env, mooring, shell)
            }
        };
        stamp_focus(focus, comp.span)
    }

    fn step_case(
        &mut self,
        scrutinee: &crate::source::Spanned<Val>,
        arms: &[CaseArm],
        env: &Env,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Focus {
        let (label, payload) = match close(&scrutinee.item, env) {
            Ok(Value::Variant { label, payload }) => (label, payload),
            Ok(other) => {
                return Focus::Halt(Break::Error(shell.err(
                    format!(
                        "case: scrutinee must be a variant, got {} {}",
                        other.type_name(),
                        other
                    ),
                    1,
                )));
            }
            Err(e) => return Focus::Halt(Break::Error(e)),
        };
        let Some(arm) = arms.iter().find(|arm| arm.tag.item == label) else {
            let handled: Vec<String> = arms
                .iter()
                .map(|a| crate::syntax::tag::tag_row_label(&a.tag.item))
                .collect();
            return Focus::Halt(Break::Error(shell.err(
                format!(
                    "case: no arm for variant `{label}`; this case matches: {}",
                    handled.join(", ")
                ),
                1,
            )));
        };
        let payload = payload.map_or(Value::Unit, |p| *p);
        match pattern::bind_pattern(&arm.pattern, &payload, &[], env.clone(), mooring, shell) {
            Ok(env2) => Focus::Eval(Closure {
                comp: Arc::clone(arm.body.comp()),
                env: env2,
            }),
            Err(b) => Focus::Halt(b),
        }
    }

    fn step_exec(
        &mut self,
        exec: &crate::ir::Exec,
        span: Option<Span>,
        env: &Env,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Focus {
        if let Err(b) = crate::process::check(mooring) {
            return Focus::Halt(b);
        }
        let argv = match close_args(&exec.args, env) {
            Ok(v) => v,
            Err(e) => return Focus::Halt(Break::Error(e)),
        };
        let redirs = match close_redirects(&exec.redirects, env) {
            Ok(v) => v,
            Err(e) => return Focus::Halt(Break::Error(e)),
        };
        if !matches!(exec.head.name().bare(), Some(name) if name.starts_with('_')) {
            shell.local.audit.call_site = span;
        }
        let resolution = match command_call::classify_command(&exec.head, env, mooring, shell) {
            Ok(r) => r,
            Err(b) => return Focus::Halt(b),
        };
        match resolution {
            Resolution::Env(v) => {
                if let Err(b) = self.push_redirect(redirs, mooring, shell) {
                    return Focus::Halt(b);
                }
                if argv.is_empty() {
                    self.force(v, env, mooring, shell)
                } else {
                    self.apply_rule(v, argv, env.clone(), span, mooring, shell)
                }
            }
            Resolution::Handler { entry, depth } => {
                if let Err(b) = self.push_redirect(redirs, mooring, shell) {
                    return Focus::Halt(b);
                }
                if let Err(b) = self.reserve(shell) {
                    return Focus::Halt(b);
                }
                let frame = Box::new(shell.context.handlers.strip_matched(depth));
                self.push(Frame::Unmask { frame });
                let call_args = render_handler_args(&entry.name, entry.arity, &argv);
                self.apply_rule(entry.thunk.clone(), call_args, env.clone(), span, mooring, shell)
            }
            Resolution::Base(entry) => {
                match command_call::run_base_frame(&entry, &argv, &redirs, env, mooring, shell) {
                    Ok(v) => Focus::Return(Terminal::Value(v)),
                    Err(b) => Focus::Halt(b),
                }
            }
            Resolution::External(id) => {
                match command_call::run_external(id, &argv, &redirs, env, mooring, shell) {
                    Ok(v) => Focus::Return(Terminal::Value(v)),
                    Err(b) => Focus::Halt(b),
                }
            }
        }
    }

    // ── §2.3: frames, and their two rules each ──────────────────────────

    fn step_return(&mut self, frame: Frame, t: Terminal, mooring: &Mooring, shell: &mut Shell) -> Focus {
        match frame {
            Frame::To { bind, env, prev_stdout } => {
                shell.io.stdout = prev_stdout;
                let v = match as_value(t) {
                    Ok(v) => v,
                    Err(b) => return Focus::Halt(b),
                };
                set_status(&v, shell);
                let CompKind::Bind { pattern, rest, .. } = &bind.item else {
                    unreachable!("a To frame's `bind` is always a Bind comp")
                };
                match pattern::bind_pattern(pattern, &v, &[], env, mooring, shell) {
                    Ok(env2) => Focus::Eval(Closure {
                        comp: Arc::clone(rest),
                        env: env2,
                    }),
                    Err(b) => Focus::Halt(stamp(b, bind.span)),
                }
            }

            Frame::Chain { .. } => Focus::Return(t),

            Frame::Apply { args, env, span } => match t {
                Terminal::Lambda(c) => stamp_focus(self.beta(c, args, env, span, mooring, shell), span),
                Terminal::Value(v) => {
                    stamp_focus(self.apply_rule(v, args, env, span, mooring, shell), span)
                }
            },

            Frame::Capture { prev, buf, span } => {
                shell.io.stdout = prev;
                let bytes = io::take_buffer(&buf);
                let overflowed = io::buffer_overflowed(&buf);
                let v = match as_value(t) {
                    Ok(v) => v,
                    Err(b) => return Focus::Halt(stamp(b, span)),
                };
                assert!(
                    matches!(v, Value::Unit),
                    "capture's operand is result: Bytes, so WF-2 makes its value Unit"
                );
                if overflowed {
                    if !bytes.is_empty()
                        && let Err(e) = shell.write_ambient(&bytes)
                    {
                        return Focus::Halt(Break::Error(shell.err(format!("capture flush: {e}"), 1)));
                    }
                    return Focus::Halt(stamp(
                        Break::Error(
                            Error::new(
                                format!(
                                    "capture exceeded {} MiB — the bytes that fit went out to the \
                                     visible stream rather than into the value, because a prefix is \
                                     not what the command wrote",
                                    io::SINK_BUFFER_CAP / (1024 * 1024)
                                ),
                                1,
                            )
                            .with_hint("did you mean to write this to a file? `cmd > out.txt` keeps every byte"),
                        ),
                        span,
                    ));
                }
                Focus::Return(Terminal::Value(Value::Bytes(bytes)))
            }

            Frame::Decode { span } => {
                let v = match as_value(t) {
                    Ok(v) => v,
                    Err(b) => return Focus::Halt(stamp(b, span)),
                };
                let Value::Bytes(mut bytes) = v else {
                    return Focus::Halt(stamp(
                        Break::Error(
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
                        ),
                        span,
                    ));
                };
                io::strip_trailing_newline(&mut bytes);
                match crate::builtins::util::decode_utf8_strict(
                    bytes,
                    "captured output is not valid UTF-8",
                    "bind with `| from-bytes` to keep raw output",
                ) {
                    Ok(text) => Focus::Return(Terminal::Value(Value::String(text))),
                    Err(b) => Focus::Halt(stamp(b, span)),
                }
            }

            Frame::Source { rest, env, span } => {
                let v = match as_value(t) {
                    Ok(v) => v,
                    Err(b) => return Focus::Halt(stamp(b, span)),
                };
                let Value::String(p) = v else {
                    return Focus::Halt(stamp(
                        Break::Error(shell.err_hint(
                            format!("source: expected String, got {}", v.type_name()),
                            "the path to `source` must be a computation of type F String",
                            1,
                        )),
                        span,
                    ));
                };
                match crate::builtins::modules::source(&p, env, super::Mode::Local, span, mooring, shell) {
                    Ok(ran) => match ran.outcome {
                        Ok(_) => Focus::Eval(Closure {
                            comp: rest,
                            env: ran.env,
                        }),
                        Err(s) => Focus::Halt(s),
                    },
                    Err(b) => Focus::Halt(stamp(b, span)),
                }
            }

            Frame::Redirect(state) => {
                let mut state = *state;
                let commits = state.tear_down(shell);
                match state.settle_writes(WriteFate::Commit, mooring, shell) {
                    Ok(_unfinished) => match command::commit_atomics(commits) {
                        Ok(()) => Focus::Return(t),
                        Err(b) => Focus::Halt(b),
                    },
                    Err(b) => Focus::Halt(b),
                }
            }

            Frame::Unmask { frame } => {
                shell.context.handlers.restore_matched(*frame);
                Focus::Return(t)
            }

            Frame::Try { scope, saved, .. } => {
                let _children = shell.local.audit.close(scope);
                shell.local.audit.set_capture(saved);
                shell.last_status = 0;
                Focus::Return(t)
            }

            Frame::Guard { cleanup, env } => {
                let v = match as_value(t) {
                    Ok(v) => v,
                    Err(b) => return Focus::Halt(b),
                };
                if let Err(b) = self.reserve(shell) {
                    return Focus::Halt(b);
                }
                self.push(Frame::Cleanup { outcome: Ok(v) });
                self.force(cleanup, &env, mooring, shell)
            }

            Frame::Cleanup { outcome } => match outcome {
                Ok(v) => Focus::Return(Terminal::Value(v)),
                Err(s) => Focus::Halt(s),
            },

            Frame::Within(undo) => {
                undo.apply(shell);
                Focus::Return(t)
            }

            Frame::Grant => {
                shell.context.grants.pop();
                Focus::Return(t)
            }

            Frame::Audit { scope, saved } => {
                let children = shell.local.audit.close(scope);
                shell.local.audit.set_capture(saved);
                let status = shell.last_status;
                let v = match as_value(t) {
                    Ok(v) => v,
                    Err(b) => return Focus::Halt(b),
                };
                Focus::Return(Terminal::Value(tree_value(status, v, None, &children)))
            }
        }
    }

    fn step_halt(&mut self, frame: Frame, s: Break, mooring: &Mooring, shell: &mut Shell) -> Focus {
        match frame {
            Frame::To { prev_stdout, .. } => {
                shell.io.stdout = prev_stdout;
                let s = if let Break::Error(e) = s {
                    if e.hint.is_none() {
                        Break::Error(e.with_hint(
                            "later steps in this block did not run; wrap a step in `attempt` if its \
                             failure should not stop the rest",
                        ))
                    } else {
                        Break::Error(e)
                    }
                } else {
                    s
                };
                Focus::Halt(s)
            }

            Frame::Chain { node, next, env } => match s {
                Break::Error(e) => {
                    shell.last_status = e.exit_code();
                    let CompKind::Chain(parts) = &node.item else {
                        unreachable!("a Chain frame's node is always a Chain comp")
                    };
                    if next < parts.len() {
                        let part = Arc::clone(&parts[next]);
                        if let Err(b) = crate::process::check(mooring) {
                            return Focus::Halt(b);
                        }
                        self.push(Frame::Chain { node, next: next + 1, env: env.clone() });
                        Focus::Eval(Closure { comp: part, env })
                    } else {
                        Focus::Halt(Break::Error(e))
                    }
                }
                Break::Escape(esc) => Focus::Halt(Break::Escape(esc)),
            },

            Frame::Apply { .. } => Focus::Halt(s),

            Frame::Capture { prev, buf, .. } => {
                shell.io.stdout = prev;
                let bytes = io::take_buffer(&buf);
                if !bytes.is_empty()
                    && let Err(e) = shell.write_ambient(&bytes)
                {
                    return Focus::Halt(Break::Error(shell.err(format!("capture flush: {e}"), 1)));
                }
                Focus::Halt(s)
            }

            Frame::Decode { .. } => Focus::Halt(s),

            Frame::Source { .. } => Focus::Halt(s),

            Frame::Redirect(state) => {
                let mut state = *state;
                let fate = if s.is_stop() { WriteFate::Defer } else { WriteFate::Abort };
                let mut commits = state.tear_down(shell);
                let settled = state.settle_writes(fate, mooring, shell);
                commits.extend(settled.unwrap_or_default());
                Focus::Halt(command::defer_to_stop(s, commits))
            }

            Frame::Unmask { frame } => {
                shell.context.handlers.restore_matched(*frame);
                Focus::Halt(s)
            }

            Frame::Try { handler, env, scope, saved } => match s {
                Break::Error(e) => {
                    let children = shell.local.audit.close(scope);
                    shell.local.audit.set_capture(saved);
                    let outcome = classify(&e, &children, shell);
                    let record = error_record(&outcome.cmd, outcome.status, &outcome.message, outcome.line, outcome.col);
                    shell.last_status = 0;
                    self.apply_rule(handler, vec![record], *env, None, mooring, shell)
                }
                Break::Escape(esc) => {
                    let _children = shell.local.audit.close(scope);
                    shell.local.audit.set_capture(saved);
                    Focus::Halt(Break::Escape(esc))
                }
            },

            Frame::Guard { cleanup, env } => {
                if let Err(b) = self.reserve(shell) {
                    return Focus::Halt(b);
                }
                self.push(Frame::Cleanup { outcome: Err(s) });
                self.force(cleanup, &env, mooring, shell)
            }

            Frame::Cleanup { .. } => Focus::Halt(s),

            Frame::Within(undo) => {
                undo.apply(shell);
                Focus::Halt(s)
            }

            Frame::Grant => {
                shell.context.grants.pop();
                Focus::Halt(s)
            }

            Frame::Audit { scope, saved } => match s {
                Break::Error(e) => {
                    let children = shell.local.audit.close(scope);
                    shell.local.audit.set_capture(saved);
                    shell.last_status = e.exit_code();
                    Focus::Return(Terminal::Value(tree_value(
                        e.exit_code(),
                        Value::Unit,
                        Some(e.message_with_hint()),
                        &children,
                    )))
                }
                Break::Escape(esc) => {
                    let _children = shell.local.audit.close(scope);
                    shell.local.audit.set_capture(saved);
                    Focus::Halt(Break::Escape(esc))
                }
            },
        }
    }
}

impl Frame {
    /// The panic path: undo what a checkpoint cannot hold — fds, staging
    /// files, audit scopes (§2.6). `To`/`Capture` restore `io.stdout`;
    /// `Redirect` as its own rule; `Unmask` restores; `Try`/`Audit`
    /// `audit.close(scope)` then `set_capture(saved)`; `Within` applies its
    /// undo; `Grant` pops. `Chain`, `Apply`, `Decode`, `Source`, `Guard`,
    /// `Cleanup` do nothing.
    fn abandon(self, shell: &mut Shell) {
        match self {
            Frame::To { prev_stdout, .. } | Frame::Capture { prev: prev_stdout, .. } => {
                shell.io.stdout = prev_stdout;
            }
            Frame::Redirect(state) => state.abandon(shell),
            Frame::Unmask { frame } => shell.context.handlers.restore_matched(*frame),
            Frame::Try { scope, saved, .. } | Frame::Audit { scope, saved } => {
                let _children = shell.local.audit.close(scope);
                shell.local.audit.set_capture(saved);
            }
            Frame::Within(undo) => undo.apply(shell),
            Frame::Grant => {
                shell.context.grants.pop();
            }
            Frame::Chain { .. }
            | Frame::Apply { .. }
            | Frame::Decode { .. }
            | Frame::Source { .. }
            | Frame::Guard { .. }
            | Frame::Cleanup { .. } => {}
        }
    }
}

// ── Boundaries (§2.1) ───────────────────────────────────────────────────

/// `Return(Lambda(_))` at the end of a run: unreachable for a checked
/// program (S3, §3.5), kept as a clean error rather than a panic.
fn end_of_run(focus: Focus) -> Settled<Value> {
    match focus {
        Focus::Return(Terminal::Value(v)) => Ok(v),
        Focus::Return(Terminal::Lambda(_)) => Err(Break::Error(bare_lambda_error())),
        Focus::Halt(s) => Err(s),
        Focus::Eval(_) => {
            unreachable!("the loop only stops when the stack is empty and focus is a terminal or a halt")
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
    let mut m = Machine {
        focus: Focus::Return(Terminal::Value(Value::Unit)),
        stack: Vec::new(),
    };
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

/// `inject`, then step until the stack is empty.
pub(crate) fn evaluate(closure: Closure, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    run(|m, _, _| *m = Machine::inject(closure), mooring, shell)
}

/// The same, from a closed value meeting arguments — `Machine::applying`
/// (§2.4) sets the first state.
pub(crate) fn apply(f: Value, args: Vec<Value>, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    run(
        |m, mooring, shell| {
            let env = shell.env.clone();
            *m = Machine::applying(f, args, env, mooring, shell);
        },
        mooring,
        shell,
    )
}

/// `force v` on a value already in hand (a host door — the hook table's
/// arity-0 entries — rather than a `CompKind::Force` node): `force(thunk M)
/// = M`, run as its own machine; an arity-0 native runs; any other native
/// stands as itself.  Unreachable on a `Thunk` whose body is a bare `Lam`
/// for a checked program (S3 rules it out of a `Force` position), so this
/// mirrors `Machine::force`'s rule without that unreachable arm's `Focus`
/// plumbing.
pub(crate) fn force(v: Value, env: &Env, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    match v {
        Value::Thunk(c) => evaluate(c, mooring, shell),
        Value::Native { entry, applied } if entry.fixed_arity() == 0 => {
            let v = super::audit::run_native(&entry, &applied, env, mooring, shell)?;
            set_status(&v, shell);
            Ok(v)
        }
        native @ Value::Native { .. } => Ok(native),
        other => Err(Break::Error(shell.err_hint(
            format!("cannot force {}: ! requires a Block", other.type_name()),
            "wrap in a block: !{ expr }",
            1,
        ))),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs scaffolding for the sourced-file-halt test"
)]
mod tests {
    use super::*;
    use crate::evaluator::with_capture;
    use crate::ir::{Phrase, Toplevel};
    use crate::source::{FileId, Spanned};
    use crate::types::Break;

    fn toplevel(source: &str) -> Toplevel {
        let ast = crate::syntax::parser::parse_with(source, FileId::DUMMY).expect("parse");
        let top = crate::elaborator::elaborate(&ast, std::collections::HashSet::default(), "<test>")
            .expect("elaborate");
        crate::typecheck::typecheck(&top, crate::typecheck::SessionSchemes::default()).expect("typecheck")
    }

    /// Run every phrase of `source` through the machine: a `Define` binds
    /// into the running `Env` with `pattern::bind_pattern`, a `Run` phrase's
    /// value is the previous one, mirroring `run_phrases`'s shape without
    /// its lease/PATH-shadow machinery, irrelevant to what these tests probe.
    fn run(source: &str, shell: &mut Shell) -> Settled<Value> {
        run_with(source, &Mooring::adrift(), shell)
    }

    fn run_with(source: &str, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
        let top = toplevel(source);
        let mut env = shell.env.clone();
        let mut value = Ok(Value::Unit);
        for phrase in &top.phrases {
            match &phrase.item {
                Phrase::Run(comp) => {
                    value = evaluate(Closure { comp: comp.clone(), env: env.clone() }, mooring, shell);
                    value.as_ref().map_err(Clone::clone)?;
                }
                Phrase::Define { pattern, comp, .. } => {
                    let v = evaluate(Closure { comp: comp.clone(), env: env.clone() }, mooring, shell)?;
                    env = pattern::bind_pattern(pattern, &v, &[], env, mooring, shell)?;
                    value = Ok(Value::Unit);
                }
                Phrase::Source { .. } => unreachable!("no test source uses top-level `source`"),
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
        let out = run(
            "if true { let inner = 1; return $inner }\nreturn $inner",
            &mut shell,
        );
        match out {
            Err(Break::Error(e)) => assert!(
                e.message.contains("undefined variable"),
                "expected an undefined-variable error, got {:?}",
                e.message
            ),
            other => panic!("expected `inner` to be out of extent, got {other:?}"),
        }
    }

    /// A binder's RHS writes to ambient (visible), the tail to the block's
    /// own stdout — on both the return path and the halt path.
    #[test]
    fn bind_rhs_writes_ambient_tail_writes_stdout() {
        let mut shell = new_shell();
        let (result, bytes, _overflowed) = with_capture(&mut shell, |shell| {
            run("echo rhs; echo tail", shell)
        });
        result.expect("both echoes must succeed");
        assert_eq!(bytes, b"rhs\ntail\n");
    }

    #[test]
    fn bind_rhs_writes_ambient_even_when_the_tail_halts() {
        let mut shell = new_shell();
        let (result, bytes, _overflowed) = with_capture(&mut shell, |shell| {
            run("echo rhs; exit 3", shell)
        });
        assert!(result.is_err(), "exit 3 must halt");
        assert_eq!(bytes, b"rhs\n");
    }

    /// `a ? b ? c`: an `Error` arm falls through to the next; an `Escape`
    /// propagates without trying a fallback.
    #[test]
    fn chain_falls_through_on_error_and_propagates_escape() {
        let mut shell = new_shell();
        run("false ? true", &mut shell).expect("second arm must run");

        let mut shell = new_shell();
        let out = run("exit 5 ? true", &mut shell);
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
        let args: Args = vec![Spanned::with_span(
            Some(span),
            ValListElem::Single(Val::Int(1)),
        )];
        let app = Spanned::with_span(Some(span), CompKind::App { head, args });
        let closure = Closure {
            comp: Arc::new(app),
            env: shell.env.clone(),
        };
        let out = evaluate(closure, &Mooring::adrift(), &mut shell);
        match out {
            Err(Break::Error(e)) => {
                assert!(e.message.contains("is not a function"), "got {:?}", e.message);
                assert_eq!(e.span, Some(span), "the over-application error must carry the call's span");
            }
            other => panic!("expected a not-a-function error, got {other:?}"),
        }
    }

    /// A curried call over a thunked partial application: `g 2` with
    /// `g = thunk (f 1)`.
    #[test]
    fn curried_call_over_a_thunked_partial_application() {
        let mut shell = new_shell();
        let out = run(
            "let f = { |a b| return $[$a + $b] }\nlet g = f 1\ng 2",
            &mut shell,
        )
        .expect("curried call must succeed");
        assert!(matches!(out, Value::Int(3)), "expected 3, got {out:?}");
    }

    /// `apply` — the boundary §4 natives reach `machine::apply` through —
    /// meeting a closed lambda value directly, with no `Comp` in sight.
    #[test]
    fn apply_meets_a_closed_lambda_value() {
        let mut shell = new_shell();
        let f = run("{ |a b| return $[$a + $b] }", &mut shell).expect("closure must close");
        let mooring = Mooring::adrift();
        let out = apply(f, vec![Value::Int(1), Value::Int(2)], &mooring, &mut shell)
            .expect("apply must succeed");
        assert!(matches!(out, Value::Int(3)), "expected 3, got {out:?}");
    }

    /// A two-member mutually recursive group.
    #[test]
    fn two_member_recursive_group() {
        let mut shell = new_shell();
        let out = run(
            "let even = { |n| if $[$n == 0] { return true } else { odd $[$n - 1] } }\n\
             let odd = { |n| if $[$n == 0] { return false } else { even $[$n - 1] } }\n\
             even 10",
            &mut shell,
        )
        .expect("mutual recursion must terminate");
        assert!(matches!(out, Value::Bool(true)), "expected true, got {out:?}");
    }

    /// A `source`d file's halt halts the block that sourced it.
    #[test]
    fn sourced_files_halt_halts_the_block() {
        let dir = std::env::temp_dir().join(format!("ral-machine-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("halts.ral");
        std::fs::write(&path, "let before = 1\nexit 9\n").expect("write");
        let mut shell = new_shell();
        // A block-local `source` (as opposed to a top-level phrase) so the
        // program elaborates through `CompKind::Source`/`Frame::Source`
        // rather than `Phrase::Source`, which `run_phrases` — not the
        // machine — owns.
        let src = format!("!{{ source '{}'; return 1 }}", path.display());
        let out = run(&src, &mut shell);
        assert!(out.is_err(), "the sourced file's exit must halt the block");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
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
        let out = run(
            "let f = { |n| if $[$n <= 0] { return 0 } else { let prev = f $[$n - 1]; return $[$n + $prev] } }\nf 100",
            &mut shell,
        );
        match out {
            Err(Break::Error(e)) => {
                assert!(e.message.contains("recursion limit exceeded"), "got {:?}", e.message);
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
        assert!(out.is_err(), "a cancelled tight loop must halt rather than spin");
    }

    /// §2.1/§4: a native (`map`) applying a function that itself calls `map`
    /// nests one host stack frame's worth of `machine::run` per level.  That
    /// must fit the 2 MiB default stack a worker thread gets
    /// (`Shell::spawn_thread`, `scope.rs:4-6`) up to `NESTED_MACHINE_LIMIT`,
    /// and stop there with the depth-exceeded error rather than a raw
    /// overflow — run on a fresh thread with no explicit stack size, so it
    /// inherits the platform default rather than the test harness thread's.
    #[test]
    fn nested_machines_fit_a_worker_stack() {
        let handle = std::thread::spawn(|| {
            let mut shell = new_shell();
            let out = run(
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
