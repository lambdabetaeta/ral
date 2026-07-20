//! Non-interactive execution for script, stdin, and `-c` modes.

use ral_core::transport::{Program, Turn};
use ral_core::types::{Break, Escape, Settled};
use ral_core::{
    RequestedTerminalAccess, TurnIo, TurnReport, TurnRequest, TurnStdin, diagnostic,
    elaborator::elaborate, syntax::parser::parse,
};
use std::process::ExitCode;
use std::sync::Arc;

use crate::PRELUDE;
use crate::cli::{BatchOpts, RunOpts};
use crate::platform::{apply_session_capabilities, exit_byte, load_exit_hints, probe_terminal};

/// Serialise the execution tree root to JSON and emit it on stderr.
fn emit_audit_tree(
    name: &str,
    result: &Settled<ral_core::types::Value>,
    exit_code: i32,
    tree_children: Vec<ral_core::types::ExecNode>,
    audit_start: i64,
    pretty: bool,
    principal: String,
) {
    use ral_core::types::{AuditIo, AuditTime, CallSite, ExecNode, Value};
    let (value, err_msg) = match result {
        Ok(v) => (v.clone(), String::new()),
        Err(Break::Error(e)) => (Value::Unit, e.message.clone()),
        Err(Break::Escape(_)) => (Value::Unit, String::new()),
    };
    let root = ExecNode::command(
        name,
        Vec::new(),
        exit_code,
        CallSite {
            script: name.to_string(),
            line: 0,
            col: 0,
        },
        AuditIo {
            stdout: Vec::new(),
            stderr: err_msg.into_bytes(),
        },
        value,
        tree_children,
        AuditTime {
            start: audit_start,
            end: ral_core::types::epoch_us(),
        },
        principal,
    );
    let json_val = ral_core::builtins::value_to_json_lossy_bytes(&root.to_value());
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

    if check {
        let comp = elaborate(&ast, std::collections::HashSet::default());
        if let Err(errors) = run_check(&comp) {
            eprint!(
                "{}",
                diagnostic::format_type_errors_ariadne(name, source, &errors)
            );
            return ExitCode::from(1u8);
        }
        return ExitCode::SUCCESS;
    }

    let mut shell = ral_core::driver::boot_shell(terminal, &PRELUDE, &host_surface);
    shell.set_exit_hints(load_exit_hints());
    tick!("builtins");
    if let Some(n) = recursion_limit {
        shell.set_recursion_limit(n);
    }
    shell.install_root_context(name.to_string(), source);
    shell.set_args(script_args);

    if let Err(code) = apply_session_capabilities(&mut shell, &capabilities) {
        return code;
    }
    tick!("caps");

    let audit_start = if audit {
        ral_core::types::epoch_us()
    } else {
        0
    };

    if audit {
        shell.enable_audit();
    }

    let bare = elaborate(&ast, std::collections::HashSet::default());
    tick!("elaborate");

    let comp = match run_check(&bare) {
        Ok(annotated) => Arc::new(annotated),
        Err(errs) => {
            eprint!(
                "{}",
                diagnostic::format_type_errors_ariadne(name, source, &errs)
            );
            return ExitCode::from(1u8);
        }
    };
    tick!("typecheck");

    let terminal_access = if shell.terminal().startup_foreground {
        RequestedTerminalAccess::Leased
    } else {
        RequestedTerminalAccess::Denied
    };
    let result = match shell.run_turn(TurnRequest {
        turn: Turn {
            program: Program::Source(source.to_string()),
            script_name: name.to_string(),
            caps: ral_core::types::Capabilities::root(),
            turn_limit: None,
            deferred_lease: None,
            worker_cap: None,
            io: TurnIo::Inherit,
            terminal: terminal_access,
            stdin: TurnStdin::Inherit,
        },
        surface: None,
        deferred: None,
        desk: None,
        nursery: None,
        lifecycle: Box::new(()),
    }) {
        TurnReport::Ran { result, .. } => result,
        // Batch already typechecked above, so a static report should not occur
        // here; treat it defensively as a fatal run (exit 1).
        TurnReport::Static { .. } => Err(Break::Escape(Escape::Exit(1))),
    };
    tick!("evaluate");

    let tree_children = if audit {
        shell.take_audit_fragment().into_nodes()
    } else {
        Vec::new()
    };

    let exit_code = match &result {
        Ok(_) => shell.last_status().clamp(0, 255),
        Err(Break::Escape(Escape::Exit(code))) => (*code).clamp(0, 255),
        Err(Break::Error(e)) => {
            let single_command = ral_core::ir::is_single_command(&comp);
            if audit {
                diagnostic::report_runtime_error(
                    &mut std::io::sink(),
                    shell.sources(),
                    e,
                    single_command,
                )
            } else {
                diagnostic::report_runtime_error(
                    &mut std::io::stderr(),
                    shell.sources(),
                    e,
                    single_command,
                )
            }
        }
        #[cfg(unix)]
        Err(Break::Escape(Escape::Stopped { .. })) => 1,
    };

    if audit {
        emit_audit_tree(
            name,
            &result,
            exit_code,
            tree_children,
            audit_start,
            pretty,
            shell.principal(),
        );
    }

    ExitCode::from(exit_byte(exit_code))
}
