//! Single-input parse / typecheck / evaluate cycle, routed through the
//! transport seam.
//!
//! [`step`] is the per-line entry point.  It dispatches a source run
//! through the [`IdentityTransport`] and drains the event stream for the
//! terminal [`Report`].  Lifecycle hooks (`pre-exec`, `chpwd`,
//! `post-exec`) fire around the dispatch through
//! [`IdentityTransport::with_shell`].
//!
//! Job-control and plugin-lifecycle commands are handled by the captured
//! builtins installed at boot (see [`super::host_handlers`]).

use ral_core::transport::{self, Diagnostics, IdentityTransport, Program, Report, Run};
use ral_core::{RequestedTerminalAccess, RunIo, RunStdin};
use ral_core::{Value, builtins};
use std::sync::{Arc, Mutex};

use super::plugin::{PluginRuntime, run_lifecycle_hook};

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

/// Fire the `pre-exec` lifecycle hook before a dispatch.
fn pre_exec(runtime: &Arc<Mutex<PluginRuntime>>, shell: &mut ral_core::Shell, src: &str) {
    run_lifecycle_hook(
        runtime,
        &ral_core::types::Mooring::adrift(),
        shell,
        "pre-exec",
        &[Value::map(vec![(
            "src".into(),
            Value::String(src.to_string()),
        )])],
    );
}

/// Drain a pending `chpwd` then fire `post-exec` after a dispatch — both
/// side-effects; neither redefines the run status, which the transport
/// already computed.
fn post_exec(
    runtime: &Arc<Mutex<PluginRuntime>>,
    shell: &mut ral_core::Shell,
    src: &str,
    status: i32,
) {
    if let Some((old, new)) = shell.repl_mut().pending_chpwd.take() {
        run_lifecycle_hook(
            runtime,
            &ral_core::types::Mooring::adrift(),
            shell,
            "chpwd",
            &[Value::map(vec![
                (
                    "old".into(),
                    Value::String(old.to_string_lossy().into_owned()),
                ),
                (
                    "new".into(),
                    Value::String(new.to_string_lossy().into_owned()),
                ),
            ])],
        );
    }
    run_lifecycle_hook(
        runtime,
        &ral_core::types::Mooring::adrift(),
        shell,
        "post-exec",
        &[Value::map(vec![
            ("src".into(), Value::String(src.to_string())),
            ("status".into(), Value::Int(i64::from(status))),
        ])],
    );
}

/// Parse, typecheck, and evaluate one trimmed REPL input through the
/// transport, running pre-exec and post-exec hooks around evaluation.
/// Returns `Some(code)` when the shell should exit.
///
/// `job_table` is threaded as the shared `Arc<Mutex<…>>` rather than a
/// held lock: the captured builtins (`fg`, `bg`, `disown`, `jobs`)
/// take their own short-lived lock during evaluation, so holding the
/// guard across the evaluator body would self-deadlock.  The only
/// post-eval mutation — recording a stopped job — locks just for the
/// `jt.add` call.
pub(super) fn execute_input(
    trimmed: &str,
    transport: &IdentityTransport,
    #[cfg(unix)] job_table: &Arc<Mutex<crate::jobs::JobTable>>,
    runtime: &Arc<Mutex<PluginRuntime>>,
    #[cfg(feature = "structural")] worksheet: &mut super::worksheet::Worksheet,
) -> Option<u8> {
    // Fire pre-exec hook through transport shell access.
    transport.with_shell(|shell| pre_exec(runtime, shell, trimmed));

    // Build the transport-level Run from the source text.
    let run = Run {
        program: Program::Source(trimmed.to_string()),
        script_name: "<stdin>".to_string(),
        caps: ral_core::types::Capabilities::root(),
        wall: None,
        deferred_lease: None,
        worker_cap: None,
        io: RunIo::Inherit,
        terminal: RequestedTerminalAccess::Leased,
        stdin: RunStdin::Inherit,
        trail: None,
    };

    // Dispatch and drain to the terminal Report.  The REPL renders no live
    // surface values and answers no enquiries; it installs no deferred sink,
    // so a session-lived batch is dropped.
    let report = transport::dispatch_to_report(
        transport,
        run,
        |_val| {},
        |_req| Err(transport::EnquiryError::no_desk()),
    );

    let Some(report) = report else {
        eprintln!("ral: internal error: dispatch completed without a Report");
        return None;
    };

    match report {
        Report::Static { diagnostics } => {
            match diagnostics {
                Diagnostics::Parse(msg) => {
                    eprintln!("parse error: {msg}");
                }
                Diagnostics::Types(errs) => {
                    for e in &errs {
                        eprintln!("{e}");
                    }
                }
                Diagnostics::Host(msg) => {
                    eprintln!("{msg}");
                }
            }
            None
        }
        Report::Ran { ending, .. } => {
            let status = ending.status();
            let exit_code = match ending {
                transport::Ending::Settled { value, .. } => {
                    print_result(&Value::from(value));
                    // The run installed its bindings: record their dependency
                    // edges and effect verdict into the worksheet model.
                    #[cfg(feature = "structural")]
                    transport.with_shell(|shell| worksheet.record(trimmed, shell));
                    None
                }
                transport::Ending::Raised { rendered, .. }
                | transport::Ending::Walled { rendered, .. } => {
                    eprint!("{rendered}");
                    None
                }
                transport::Ending::Exited(code) => Some(crate::platform::exit_byte(code)),
                #[cfg(unix)]
                transport::Ending::Stopped {
                    pgid,
                    signal_name,
                    pending,
                    ..
                } => {
                    let id = job_table.lock().unwrap().add(
                        pgid,
                        trimmed.to_string(),
                        crate::jobs::JobState::Stopped,
                        pending,
                    );
                    eprintln!("[{id}] stopped\t{trimmed} ({signal_name})");
                    None
                }
            };

            // Fire post-exec hook.
            transport.with_shell(|shell| post_exec(runtime, shell, trimmed, status));

            exit_code
        }
    }
}

/// Parse, typecheck, and evaluate one trimmed non-empty input line.
pub(super) fn step(
    trimmed: &str,
    transport: &IdentityTransport,
    #[cfg(unix)] job_table: &Arc<Mutex<crate::jobs::JobTable>>,
    runtime: &Arc<Mutex<PluginRuntime>>,
    #[cfg(feature = "structural")] worksheet: &mut super::worksheet::Worksheet,
) -> Step {
    match execute_input(
        trimmed,
        transport,
        #[cfg(unix)]
        job_table,
        runtime,
        #[cfg(feature = "structural")]
        worksheet,
    ) {
        Some(code) => Step::Exit(code),
        None => Step::Continue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repl::plugin::HookHealth;
    use crate::repl::plugin::manifest::LoadedPlugin;
    use ral_core::Shell;
    use ral_core::source::Span;
    use ral_core::typecheck::builtins::{fun, mk_scheme, pure, thunk};
    use ral_core::typecheck::{Scheme, Ty, Unifier};
    use ral_core::types::{BuiltinBody, BuiltinEntry, DefaultPolicy, HookName, HookSig, Mooring};
    use std::borrow::Cow;

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

    /// Parse, elaborate, and evaluate `src` into a handler value.
    fn handler(shell: &mut Shell, src: &str) -> Value {
        let ast = ral_core::syntax::parser::parse(src).unwrap();
        let comp = std::sync::Arc::new(
            ral_core::elaborator::elaborate(&ast, std::collections::HashSet::default(), "")
                .expect("elaborate"),
        );
        ral_core::evaluator::evaluate(&comp, &Mooring::adrift(), shell).expect("evaluate")
    }

    /// A dressed shell, the plugin runtime holding `p`, and the sink its
    /// builtin appends every call to.
    type Dressed = (Shell, Arc<Mutex<PluginRuntime>>, Arc<Mutex<Vec<Value>>>);

    /// Dress a shell with the sink builtin and one plugin `p` whose
    /// `hook_event` handler is compiled from `handler_src`, mirroring
    /// `register_plugin_hooks`' lifecycle registration.
    fn dressed(hook_event: &str, handler_src: &str) -> Dressed {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        shell.install_captured_builtins(&vec![sink_builtin(sink.clone())].into());

        let h = handler(&mut shell, handler_src);
        shell
            .register_hook(
                HookName::plugin("p", hook_event),
                h,
                HookSig::Lifecycle {
                    kind: hook_event.into(),
                },
                DefaultPolicy::denied(),
                Span::synthetic(),
            )
            .expect("register");

        let runtime = Arc::new(Mutex::new(PluginRuntime::default()));
        super::super::plugin::lock(&runtime)
            .plugins
            .push(LoadedPlugin {
                name: "p".into(),
                hooks: vec![hook_event.into()],
                keybindings: Vec::new(),
                bindings: Vec::new(),
                state_cell: None,
                source: Arc::from(""),
                buffer_change_health: HookHealth::default(),
            });
        (shell, runtime, sink)
    }

    /// `post-exec` dispatch hands the handler one event record carrying the
    /// source line under `src` and the exit status under `status`.
    #[test]
    fn post_exec_passes_src_and_status_in_one_event_record() {
        let (mut shell, runtime, sink) =
            dressed("post-exec", "{ |ev| record $ev[src] $ev[status] }");
        post_exec(&runtime, &mut shell, "true", 7);
        assert_eq!(
            *sink.lock().unwrap(),
            vec![Value::String("true".into()), Value::Int(7)]
        );
    }

    /// `pre-exec` dispatch hands the handler one event record carrying the
    /// source line under `src`.
    #[test]
    fn pre_exec_passes_src_in_one_event_record() {
        let (mut shell, runtime, sink) = dressed("pre-exec", "{ |ev| record $ev[src] }");
        pre_exec(&runtime, &mut shell, "ls -l");
        assert_eq!(*sink.lock().unwrap(), vec![Value::String("ls -l".into())]);
    }
}
