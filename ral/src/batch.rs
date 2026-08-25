//! Non-interactive execution for script, stdin, and `-c` modes.

use ral_core::transport::{Program, Run};
use ral_core::types::{Break, Escape, Settled};
use ral_core::{
    Ending, RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, diagnostic,
    elaborator::elaborate, syntax::parser::parse,
};
use std::process::ExitCode;

use crate::PRELUDE;
use crate::cli::{BatchOpts, RunOpts};
use crate::platform::{apply_session_capabilities, exit_byte, load_exit_hints, probe_terminal};

/// Serialise the run's outcome and collected tree to JSON and emit it on
/// stderr.  `exit_code` is the process's exit status as the host resolved
/// it, not the error's own code; an escape (`exit`) is not a failure, so it
/// records no error.
fn emit_audit_tree(
    result: &Settled<ral_core::types::Value>,
    exit_code: i32,
    fragment: ral_core::types::AuditFragment,
    pretty: bool,
) {
    use ral_core::types::{Value, tree_value};
    let (value, error) = match result {
        Ok(v) => (v.clone(), None),
        Err(Break::Error(e)) => (Value::Unit, Some(e.message.clone())),
        Err(Break::Escape(_)) => (Value::Unit, None),
    };
    let json_val = ral_core::builtins::value_to_json_lossy_bytes(&tree_value(
        exit_code,
        value,
        error,
        &fragment.into_observations(),
    ));
    let json_str = if pretty {
        serde_json::to_string_pretty(&json_val).unwrap_or_default()
    } else {
        serde_json::to_string(&json_val).unwrap_or_default()
    };
    eprintln!("{json_str}");
}

/// Execute `source` non-interactively (script or `-c` mode).
///
/// Parses, elaborates, optionally typechecks, and evaluates the program. When
/// `--audit` is active, wraps the entire execution in a traced tree and emits it
/// as JSON on stderr.
pub(crate) fn run_batch(
    name: &str,
    source: &str,
    script_args: Vec<String>,
    opts: BatchOpts,
) -> ExitCode {
    let BatchOpts {
        audit,
        pretty,
        check,
        dump_ast,
        run: RunOpts {
            recursion_limit,
            capabilities,
        },
    } = opts;
    ral_core::process::install_handlers();
    // Seed the ANSI color gate so `_ansi-ok` and the prelude ansi-* constants
    // work correctly in batch (script / -c) mode, not just the REPL.
    let (_, terminal) = probe_terminal(false);

    // RAL_TIMING is a presence probe, not a basedir.
    #[allow(clippy::disallowed_methods)]
    let timing = std::env::var_os("RAL_TIMING").is_some();
    let t0 = std::time::Instant::now();
    macro_rules! tick {
        ($label:literal) => {
            if timing {
                eprintln!(
                    "[timing] {:12} {:.3}ms",
                    $label,
                    t0.elapsed().as_secs_f64() * 1000.0
                );
            }
        };
    }

    // The batch surface: core plus `watch` (`WATCH_BUILTIN`'s doc explains
    // why it's host-installed).  One value seeds `--check`'s table and boots
    // the shell below, so the two agree by construction.
    let host_surface = ral_core::HostSurface {
        statics: vec![ral_core::builtins::WATCH_BUILTIN],
        captured: Vec::new(),
    };
    let check_table = host_surface.builtin_table();
    let run_check =
        |comp: &ral_core::ir::Comp| -> Result<ral_core::ir::Comp, Vec<ral_core::TypeError>> {
            ral_core::typecheck(
                comp,
                ral_core::SessionSchemes::from_schemes(PRELUDE.schemes(), check_table.clone()),
            )
        };

    let ast = match parse(source) {
        Ok(ast) => ast,
        Err(e) => {
            eprint!(
                "{}",
                diagnostic::format_parse_error_ariadne(name, source, &e)
            );
            return ExitCode::from(2);
        }
    };
    tick!("parse");

    if dump_ast {
        for node in &ast {
            eprintln!("{node:#?}");
        }
        return ExitCode::SUCCESS;
    }

    let bare = match elaborate(&ast, std::collections::HashSet::default(), name) {
        Ok(comp) => comp,
        Err(e) => {
            eprint!(
                "{}",
                diagnostic::format_parse_error_ariadne(name, source, &e)
            );
            return ExitCode::from(2);
        }
    };
    tick!("elaborate");

    if check {
        if let Err(errors) = run_check(&bare) {
            eprint!(
                "{}",
                diagnostic::format_type_errors_ariadne(name, source, &errors)
            );
            return ExitCode::from(1u8);
        }
        return ExitCode::SUCCESS;
    }

    let mut shell = ral_core::boot::boot_shell(terminal, &PRELUDE, &host_surface);
    // The script owns this process's signals: SIGINT interrupts its run,
    // SIGTERM/SIGHUP terminate the session.
    shell.face_signals();
    shell.set_exit_hints(load_exit_hints());
    tick!("builtins");
    if let Some(n) = recursion_limit {
        shell.set_recursion_limit(n);
    }
    shell.set_args(script_args);

    if let Err(code) = apply_session_capabilities(&mut shell, &capabilities) {
        return code;
    }
    tick!("caps");

    if audit {
        shell.enable_audit();
    }

    if let Err(errs) = run_check(&bare) {
        eprint!(
            "{}",
            diagnostic::format_type_errors_ariadne(name, source, &errs)
        );
        return ExitCode::from(1u8);
    }
    tick!("typecheck");

    let terminal_access = if shell.terminal().startup_foreground {
        RequestedTerminalAccess::Leased
    } else {
        RequestedTerminalAccess::Denied
    };
    let (ending, compact_root) = match shell.run(RunRequest {
        run: Run {
            program: Program::Source(source.to_string()),
            script_name: name.to_string(),
            caps: ral_core::types::Capabilities::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Inherit,
            terminal: terminal_access,
            stdin: RunStdin::Inherit,
            trail: None,
        },
        surface: None,
        deferred: None,
        desk: None,
        fork: None,
        lifecycle: Box::new(()),
    }) {
        RunReport::Ran { ending, .. } => {
            let compact_root = match &ending {
                Ending::Raised {
                    single_command,
                    root,
                    ..
                }
                | Ending::Walled {
                    single_command,
                    root,
                    ..
                } => single_command.then_some(*root),
                _ => None,
            };
            (ending, compact_root)
        }
        // Batch already typechecked above, so a static report should not occur
        // here; treat it defensively as a fatal run (exit 1).
        RunReport::Static { .. } => (Ending::Exited(1), None),
    };
    let result = ending.into_result();
    tick!("evaluate");

    let fragment = if audit {
        shell.take_audit_fragment()
    } else {
        ral_core::types::AuditFragment::empty()
    };

    let exit_code = match &result {
        Ok(_) => shell.last_status().clamp(0, 255),
        Err(Break::Escape(Escape::Exit(code))) => (*code).clamp(0, 255),
        Err(Break::Error(e)) => {
            if audit {
                diagnostic::report_runtime_error(
                    &mut std::io::sink(),
                    shell.sources(),
                    e,
                    compact_root,
                )
            } else {
                diagnostic::report_runtime_error(
                    &mut std::io::stderr(),
                    shell.sources(),
                    e,
                    compact_root,
                )
            }
        }
        #[cfg(unix)]
        Err(Break::Escape(Escape::Stopped { .. })) => 1,
    };

    if audit {
        emit_audit_tree(&result, exit_code, fragment, pretty);
    }

    ExitCode::from(exit_byte(exit_code))
}
