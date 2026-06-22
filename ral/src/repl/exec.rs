//! Single-input parse / typecheck / evaluate cycle.
//!
//! [`step`] is the per-line entry point.  It dispatches directly to
//! [`execute_input`], which runs the parser, typechecker, evaluator, and
//! lifecycle hooks (`pre-exec`, `chpwd`, `post-exec`) and prints the
//! result.  Job-control and plugin-lifecycle commands are handled by the
//! captured builtins installed at boot (see [`super::host_handlers`]).

use ral_core::types::{Break, Escape};
use ral_core::{
    RequestedTerminalAccess, StaticDiagnostics, TurnIo, TurnReport, TurnRequest, TurnStdin,
};
use ral_core::{Shell, Value, builtins, diagnostic};
use std::sync::{Arc, Mutex};

use super::errfmt::{format_repl_parse_error, should_use_compact_parse_error};
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
                return;
            }
            println!("{}{s}", theme.value_prefix);
        }
    }
}

struct ReplLifecycle<'a> {
    runtime: &'a Arc<Mutex<PluginRuntime>>,
}

impl ral_core::TurnLifecycle for ReplLifecycle<'_> {
    fn pre_exec(&mut self, shell: &mut Shell, src: &str) {
        run_lifecycle_hook(
            self.runtime,
            shell,
            "pre-exec",
            &[Value::String(src.to_string())],
        );
    }

    fn post_exec(&mut self, shell: &mut Shell, src: &str, status: i32) {
        // chpwd drain then post-exec — both side-effects; neither redefines
        // the turn's status, which `run_source_turn` already computed.
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

/// Parse, typecheck, and evaluate one trimmed REPL input, running
/// pre-exec and post-exec hooks around the evaluation.
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
    shell: &mut Shell,
    #[cfg(unix)] job_table: &Arc<Mutex<crate::jobs::JobTable>>,
    runtime: &Arc<Mutex<PluginRuntime>>,
    #[cfg(feature = "structural")] worksheet: &mut super::worksheet::Worksheet,
) -> Option<u8> {
    let req = TurnRequest {
        script_name: "<stdin>",
        caps: ral_core::types::Capabilities::root(),
        turn_limit: None,
        detached_limit: None,
        io: TurnIo::Inherit,
        terminal: RequestedTerminalAccess::Leased,
        stdin: TurnStdin::Inherit,
        surface: None,
        boundary: None,
        lifecycle: Box::new(ReplLifecycle { runtime }),
    };
    match shell.run_source_turn(trimmed, req) {
        TurnReport::Static { diagnostics } => {
            match diagnostics {
                StaticDiagnostics::Parse(e) => {
                    if should_use_compact_parse_error(trimmed, &e.message) {
                        eprint!("{}", format_repl_parse_error(&e.message));
                    } else {
                        eprint!(
                            "{}",
                            diagnostic::format_parse_error_ariadne("<stdin>", trimmed, &e)
                        );
                    }
                }
                StaticDiagnostics::Types(errs) => {
                    eprint!(
                        "{}",
                        diagnostic::format_type_errors_ariadne("<stdin>", trimmed, &errs)
                    );
                }
            }
            None
        }
        TurnReport::Ran {
            result,
            single_command,
            ..
        } => match result {
            Ok(val) => {
                print_result(&val);
                // The turn installed its bindings: record their dependency
                // edges and effect verdict into the worksheet model, off the
                // now-updated live session.  Only a successful turn reaches
                // here, so a binding that failed to evaluate is never
                // recorded.
                #[cfg(feature = "structural")]
                worksheet.record(trimmed, shell);
                None
            }
            Err(Break::Escape(Escape::Exit(code))) => Some(code.clamp(0, 255) as u8),
            Err(Break::Error(e)) => {
                eprint!(
                    "{}",
                    diagnostic::format_runtime_error_auto(shell.sources(), &e, single_command,)
                );
                None
            }
            #[cfg(unix)]
            Err(Break::Escape(Escape::Stopped { pgid, signal, .. })) => {
                let id = job_table.lock().unwrap().add(
                    pgid.0,
                    trimmed.to_string(),
                    crate::jobs::JobState::Stopped,
                );
                eprintln!(
                    "[{id}] stopped\t{} ({})",
                    trimmed,
                    signal.name().unwrap_or("?")
                );
                None
            }
        },
    }
}

/// Parse, typecheck, and evaluate one trimmed non-empty input line.
pub(super) fn step(
    trimmed: &str,
    shell: &mut Shell,
    #[cfg(unix)] job_table: &Arc<Mutex<crate::jobs::JobTable>>,
    runtime: &Arc<Mutex<PluginRuntime>>,
    #[cfg(feature = "structural")] worksheet: &mut super::worksheet::Worksheet,
) -> Step {
    match execute_input(
        trimmed,
        shell,
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
