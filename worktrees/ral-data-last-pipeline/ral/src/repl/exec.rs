//! Single-input parse / typecheck / evaluate cycle.
//!
//! [`step`] is the per-line entry point.  It first tries the job-control
//! builtins, then falls through to [`execute_input`], which runs the
//! parser, typechecker, evaluator, and lifecycle hooks (`pre-exec`,
//! `chpwd`, `post-exec`) and prints the result.

use ral_core::{Shell, EvalSignal, Value, builtins, diagnostic, elaborate, evaluate, parse};

use super::errfmt::{format_repl_parse_error, should_use_compact_parse_error};
use super::plugin::run_lifecycle_hook;

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
            let theme = ral_core::ansi::output_theme();
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

/// Parse, typecheck, and evaluate one trimmed REPL input, running
/// pre-exec and post-exec hooks around the evaluation.
/// Returns `Some(code)` when the shell should exit.
pub(super) fn execute_input(trimmed: &str, shell: &mut Shell) -> Option<u8> {
    ral_core::signal::clear();

    match parse(trimmed) {
        Ok(ast) => {
            shell.location.source = Some(std::sync::Arc::from(trimmed));
            let bindings = shell.all_bindings().into_iter().map(|(n, _)| n).collect();
            let comp = elaborate(&ast, bindings);

            let type_errors = ral_core::typecheck(&comp, crate::baked_prelude_schemes());
            if !type_errors.is_empty() {
                for e in &type_errors {
                    eprint!(
                        "{}",
                        diagnostic::format_type_error_ariadne("<stdin>", trimmed, e)
                    );
                }
                return None;
            }

            run_lifecycle_hook(shell, "pre-exec", &[Value::String(trimmed.to_string())]);
            let eval_result = evaluate(&comp, shell);

            // Drain any pending chpwd from cd calls inside the evaluator.
            if let Some((old, new)) = shell.repl.pending_chpwd.take() {
                run_lifecycle_hook(
                    shell,
                    "chpwd",
                    &[
                        Value::String(old.to_string_lossy().into_owned()),
                        Value::String(new.to_string_lossy().into_owned()),
                    ],
                );
            }

            let status = match &eval_result {
                Ok(_) => 0i64,
                Err(EvalSignal::Error(e)) => i64::from(e.status),
                _ => 1,
            };
            run_lifecycle_hook(
                shell,
                "post-exec",
                &[Value::String(trimmed.to_string()), Value::Int(status)],
            );

            match eval_result {
                Ok(val) => print_result(&val),
                Err(EvalSignal::Exit(code)) => return Some(code.clamp(0, 255) as u8),
                Err(EvalSignal::Error(e)) => {
                    eprint!(
                        "{}",
                        diagnostic::format_runtime_error_auto(
                            "<stdin>",
                            trimmed,
                            &e,
                            comp.is_single_command(),
                        )
                    );
                }
                Err(_) => {}
            }
        }
        Err(e) => {
            if should_use_compact_parse_error(trimmed, &e.message) {
                eprint!("{}", format_repl_parse_error(&e.message));
            } else {
                eprint!(
                    "{}",
                    diagnostic::format_parse_error_ariadne(
                        "<stdin>", trimmed, e.line, e.col, &e.message,
                    )
                );
            }
        }
    }
    None
}

/// Classify, dispatch, and execute one trimmed non-empty input line.
pub(super) fn step(
    trimmed: &str,
    shell: &mut Shell,
    #[cfg(unix)] job_table: &mut crate::jobs::JobTable,
    #[cfg(unix)] stdin_tty: bool,
) -> Step {
    #[cfg(unix)]
    if super::builtins::handle_job_command(trimmed, job_table, stdin_tty) {
        return Step::Continue;
    }

    match execute_input(trimmed, shell) {
        Some(code) => Step::Exit(code),
        None => Step::Continue,
    }
}
