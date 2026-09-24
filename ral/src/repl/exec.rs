//! One input line, dispatched through the engine protocol.
//!
//! [`step`] runs a source line through the transport with the REPL host,
//! drains to its [`Report`], and fires the lifecycle hooks around it —
//! `pre-exec` before, a fresh `chpwd` then `post-exec` after, each a
//! dispatch of its own.

use ral_core::protocol::{Ending, Program, Report, Run, Transport, reading};
use ral_core::serial::FOValue;
use ral_core::serial::datum::Datum as _;
use ral_core::{RequestedTerminalAccess, RunIo, RunStdin};
use ral_core::{Value, builtins};
use std::sync::Arc;

use super::host::ReplHost;
use super::plugin::fire;

pub(super) enum Step {
    Continue,
    Exit(u8),
}

fn print_result(val: &Value) {
    match val {
        Value::Unit => {}
        Value::Bytes(b) => {
            use std::io::Write;
            let _ = std::io::stdout().write_all(b);
        }
        _ => {
            let s = match val {
                Value::List(_) | Value::Map(_) => {
                    builtins::pretty_print(val, 0, &builtins::REPL_PRINT_PARAMS)
                }
                _ => val.to_string(),
            };
            let theme = super::theme::output_theme();
            if ral_core::ansi::use_ui_color()
                && let Some(color) = &theme.value_color
            {
                println!("{color}{}{s}{}", theme.value_prefix, ral_core::ansi::RESET);
            } else {
                println!("{}{s}", theme.value_prefix);
            }
        }
    }
}

fn record(entries: Vec<(&str, FOValue)>) -> FOValue {
    FOValue::Map {
        entries: entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    }
}

/// The session's latest `cd`, if the engine will say.
fn last_chpwd(t: &dyn Transport) -> Option<ral_core::types::Chpwd> {
    reading::last_chpwd(t).ok().flatten()
}

/// An input line's run: the terminal leased, as a foreground command has it.
pub(super) fn line_run(src: &str) -> Run {
    Run {
        program: Program::Source(src.to_string()),
        script_name: "<stdin>".to_string(),
        caps: ral_core::types::GrantStack::root(),
        wall: None,
        deferred_lease: None,
        worker_cap: None,
        io: RunIo::Inherit,
        terminal: RequestedTerminalAccess::Leased,
        stdin: RunStdin::Inherit,
        trail: None,
    }
}

/// Dispatch one trimmed non-empty input line, firing the lifecycle hooks
/// around it: `post-exec` for every `pre-exec`, with the line's status, a
/// static failure's included.
pub(super) fn step(
    trimmed: &str,
    t: &dyn Transport,
    host: &Arc<ReplHost>,
    #[cfg(feature = "structural")] worksheet: &mut super::worksheet::Worksheet,
) -> Step {
    let src = trimmed.to_string().encode();
    fire(t, host, "pre-exec", &record(vec![("src", src.clone())]));
    let seen = last_chpwd(t).map(|c| c.seq);

    let (report, _) = host.dispatch(t, line_run(trimmed), None);
    let (status, step) = match report {
        Err(severed) => {
            eprintln!("ral: {severed}");
            return Step::Exit(1);
        }
        Ok(Report::Static { rendered, status }) => {
            eprint!("{rendered}");
            (status, Step::Continue)
        }
        Ok(Report::Ran { ending, .. }) => {
            let status = ending.status();
            let step = match ending {
                Ending::Settled { value, .. } => {
                    print_result(&Value::from(value));
                    #[cfg(feature = "structural")]
                    worksheet.record(
                        trimmed,
                        &reading::bind_effects(t, trimmed).unwrap_or_default(),
                    );
                    Step::Continue
                }
                Ending::Raised { rendered, .. }
                | Ending::Walled { rendered, .. }
                | Ending::Unreturnable { rendered, .. } => {
                    eprint!("{rendered}");
                    Step::Continue
                }
                Ending::Exited(code) => Step::Exit(crate::platform::exit_byte(code)),
            };
            (status, step)
        }
    };

    if let Some(ral_core::types::Chpwd { seq, old, new }) = last_chpwd(t)
        && Some(seq) != seen
    {
        fire(
            t,
            host,
            "chpwd",
            &record(vec![("old", old.encode()), ("new", new.encode())]),
        );
    }
    fire(
        t,
        host,
        "post-exec",
        &record(vec![
            ("src", src),
            (
                "status",
                FOValue::Int {
                    value: status.into(),
                },
            ),
        ]),
    );
    step
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test scaffolding")]
mod tests {
    use super::*;
    use crate::repl::plugin::PluginRuntime;
    use crate::repl::plugin::manifest::{LoadedPlugin, Manifest};
    use ral_core::protocol::IdentityTransport;
    use ral_core::source::Span;
    use ral_core::typecheck::builtins::{fun, mk_scheme, pure, thunk};
    use ral_core::typecheck::{Scheme, Ty, Unifier};
    use ral_core::types::{BuiltinBody, BuiltinEntry, DefaultPolicy, HookName, HookSig};
    use std::borrow::Cow;
    use std::sync::Mutex;

    /// The sink's type: an argv in, `Unit` out — the base-frame convention,
    /// since the sink takes whatever a hook body hands it, however much of it.
    fn sink_scheme(_u: &mut Unifier) -> Scheme {
        mk_scheme(&[], &[], &[], thunk(fun(Ty::argv(), pure(Ty::Unit))))
    }

    /// A test-only sink base frame: `record` appends its argument values into
    /// the shared vector, so a hook handler's projections out of the event
    /// record become observable.
    fn sink_builtin(sink: Arc<Mutex<Vec<Value>>>) -> BuiltinEntry {
        BuiltinEntry::base_frame(
            Cow::Borrowed("record"),
            sink_scheme,
            "record — test sink appending its arguments.",
            BuiltinBody::Captured(Arc::new(move |args, _mooring, _shell| {
                sink.lock().unwrap().extend(args.iter().cloned());
                Ok(Value::Unit)
            })),
        )
    }

    /// An engine carrying the sink builtin and one plugin `p` whose
    /// `hook_event` handler is compiled from `handler_src`, the host told of
    /// it, and the sink its builtin appends every call to.
    fn dressed(
        hook_event: &str,
        handler_src: &str,
    ) -> (IdentityTransport, Arc<ReplHost>, Arc<Mutex<Vec<Value>>>) {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let (event, handler_src) = (hook_event.to_owned(), handler_src.to_owned());
        let recorder = sink_builtin(sink.clone());
        let t = crate::repl::engine(move |shell| {
            ral_core::builtins::register(shell, crate::PRELUDE.comp());
            shell.install_captured_builtins(&vec![recorder].into());
            let h = crate::repl::eval(shell, &handler_src);
            shell
                .register_hook(
                    HookName::plugin("p", &event),
                    h,
                    HookSig::Lifecycle { kind: event },
                    DefaultPolicy::denied(),
                    Span::synthetic(),
                )
                .expect("register");
        });
        let plugin = LoadedPlugin::admit(Manifest {
            name: "p".into(),
            hooks: vec![hook_event.into()],
            keybindings: Vec::new(),
        })
        .expect("admit");
        let runtime = Arc::new(Mutex::new(PluginRuntime::default()));
        crate::repl::plugin::lock(&runtime).plugins.push(plugin);
        (t, ReplHost::new(runtime), sink)
    }

    fn run(line: &str, t: &IdentityTransport, host: &Arc<ReplHost>) {
        step(
            line,
            t,
            host,
            #[cfg(feature = "structural")]
            &mut super::super::worksheet::Worksheet::default(),
        );
    }

    /// `post-exec` hands the handler one event record carrying the source
    /// line under `src` and the line's status under `status`.
    #[test]
    fn post_exec_passes_src_and_status_in_one_event_record() {
        let (t, host, sink) = dressed("post-exec", "{ |ev| record $ev[src] $ev[status] }");
        let line = "fail [status: 7, message: 'x']";
        run(line, &t, &host);
        assert_eq!(
            *sink.lock().unwrap(),
            vec![Value::string(line), Value::Int(7)]
        );
    }

    /// A static failure still closes its `pre-exec` with a `post-exec`.
    #[test]
    fn post_exec_fires_after_a_static_failure() {
        let (t, host, sink) = dressed("post-exec", "{ |ev| record $ev[status] }");
        run("if", &t, &host);
        assert_eq!(sink.lock().unwrap().len(), 1, "one post-exec per line");
    }

    /// `pre-exec` hands the handler one event record carrying the source
    /// line under `src`.
    #[test]
    fn pre_exec_passes_src_in_one_event_record() {
        let (t, host, sink) = dressed("pre-exec", "{ |ev| record $ev[src] }");
        run("return ()", &t, &host);
        assert_eq!(*sink.lock().unwrap(), vec![Value::string("return ()")]);
    }
}
