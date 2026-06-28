//! Single-input parse / typecheck / evaluate cycle, now routed through
//! the transport seam.
//!
//! [`step`] is the per-line entry point.  It dispatches a source turn
//! through the [`IdentityTransport`] and drains the event stream for the
//! terminal [`Report`](ral_core::transport::ReportMirror).  Lifecycle
//! hooks (`pre-exec`, `chpwd`, `post-exec`) fire around the dispatch
//! through [`IdentityTransport::with_shell`].
//!
//! Job-control and plugin-lifecycle commands are still handled by the
//! captured builtins installed at boot (see [`super::host_handlers`]).

use ral_core::transport::{
    self, DiagMirror, DispatchId, Event, Frame, IdentityTransport, ReqMirror,
    ReportMirror, ResultMirror, Transport, Turn,
};
use ral_core::{
    RequestedTerminalAccess, TurnIo, TurnStdin,
};
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
                Value::List(_) | Value::Map(_) => builtins::pretty_print(val, 0),
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

/// The REPL lifecycle hooks, called through `transport.with_shell()` around
/// each dispatch.
struct ReplLifecycle<'a> {
    runtime: &'a Arc<Mutex<PluginRuntime>>,
}

impl ReplLifecycle<'_> {
    fn pre_exec(&self, shell: &mut ral_core::Shell, src: &str) {
        run_lifecycle_hook(
            self.runtime,
            shell,
            "pre-exec",
            &[Value::String(src.to_string())],
        );
    }

    fn post_exec(&self, shell: &mut ral_core::Shell, src: &str, status: i32) {
        // chpwd drain then post-exec — both side-effects; neither redefines
        // the turn status, which the transport already computed.
        if let Some((old, new)) = shell.repl_mut().pending_chpwd.take() {
            run_lifecycle_hook(
                self.runtime,
                shell,
                "chpwd",
                &[
                    Value::String(old.to_string_lossy().into_owned()),
                    Value::String(new.to_string_lossy().into_owned()),
                ],
            );
        }
        run_lifecycle_hook(
            self.runtime,
            shell,
            "post-exec",
            &[
                Value::String(src.to_string()),
                Value::Int(i64::from(status)),
            ],
        );
    }
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
    let lifecycle = ReplLifecycle { runtime };

    // Fire pre-exec hook through transport shell access.
    transport.with_shell(|shell| lifecycle.pre_exec(shell, trimmed));

    // Build the transport-level Turn from the source text.
    let req = ReqMirror {
        script_name: "<stdin>".to_string(),
        caps: ral_core::types::Capabilities::root(),
        turn_limit: None,
        detached_limit: None,
        io: TurnIo::Inherit,
        terminal: RequestedTerminalAccess::Leased,
        stdin: TurnStdin::Inherit,
    };

    // Simple counter for dispatch ids.
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = DispatchId(NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed));

    let turn = Turn::Source {
        src: trimmed.to_string(),
        req,
    };

    // Dispatch the turn synchronously.
    transport.dispatch(id, turn);

    // Drain events from the transport.  The REPL expects no Surface events
    // (surface: None today), just the terminal Report.
    let mut report: Option<ReportMirror> = None;
    while let Some(frame) = transport.events().recv() {
        match frame {
            Frame::Event(did, event) if did == id => match event {
                Event::Surface(_val) => {
                    // REPL does not render surface events today; drop.
                }
                Event::BoundarySurface(_batch) => {
                    // REPL has no detached workers with boundary; drop.
                }
                Event::Report(r) => {
                    report = Some(r);
                    break;
                }
            },
            _ => {
                // Stale or mismatched event; ignore.
            }
        }
    }

    let report = match report {
        Some(r) => r,
        None => {
            eprintln!("ral: internal error: dispatch completed without a Report");
            return None;
        }
    };

    match report {
        ReportMirror::Static { diagnostics } => {
            match diagnostics {
                DiagMirror::Parse(msg) => {
                    eprintln!("parse error: {msg}");
                }
                DiagMirror::Types(errs) => {
                    for e in &errs {
                        eprintln!("{e}");
                    }
                }
                DiagMirror::Host(msg) => {
                    eprintln!("{}", msg);
                }
            }
            None
        }
        ReportMirror::Ran {
            result,
            status,
            single_command: _single_command,
            captured: _captured,
            timed_out: _timed_out,
        } => {
            let exit_code = match result {
                ResultMirror::Ok(val) => {
                    print_result(&val);
                    // The turn installed its bindings: record their dependency
                    // edges and effect verdict into the worksheet model.
                    #[cfg(feature = "structural")]
                    transport.with_shell(|shell| worksheet.record(trimmed, shell));
                    None
                }
                ResultMirror::Err(break_mirror) => match break_mirror {
                    transport::BreakMirror::Error(msg) => {
                        eprint!("{}", msg);
                        None
                    }
                    transport::BreakMirror::Exit(code) => Some(code.clamp(0, 255) as u8),
                    #[cfg(unix)]
                    transport::BreakMirror::Stopped {
                        pgid,
                        signal: _,
                        signal_name,
                    } => {
                        let id = job_table.lock().unwrap().add(
                            pgid,
                            trimmed.to_string(),
                            crate::jobs::JobState::Stopped,
                        );
                        eprintln!("[{id}] stopped\t{trimmed} ({signal_name})");
                        None
                    }
                },
            };

            // Fire post-exec hook.
            transport.with_shell(|shell| lifecycle.post_exec(shell, trimmed, status));

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