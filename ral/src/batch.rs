//! Non-interactive execution for script, stdin, and `-c` modes.

use ral_core::protocol::{
    Ending, IdentityTransport, Program, Report, Run, Transport as _, dispatch_to_report,
};
use ral_core::serial::FOValue;
use ral_core::serial::datum::Datum as _;
use ral_core::types::{CapturePolicy, DeferredSink, GrantStack, Observation};
use ral_core::{RequestedTerminalAccess, RunIo, RunStdin, diagnostic};
use std::process::ExitCode;
use std::sync::Arc;

use crate::boot_door::{self, Boot};
use crate::cli::{BatchOpts, RunOpts};
use crate::platform::{exit_byte, local_attach, probe_terminal};
use crate::startup::engine::{BATCH, BatchConfig, INSTALLERS, batch_surface};

/// Batch's session surface: a watched worker's lines on stdout.
struct Stdout;

impl DeferredSink for Stdout {
    fn deliver(&self, batch: Vec<FOValue>) {
        for v in &batch {
            if let Some(line) = crate::surface::watch_line(v) {
                println!("{line}");
            } else if let Some(note) = crate::surface::dropped(v) {
                eprintln!("{note}");
            }
        }
    }
}

/// Run the script at `path`.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:script-read] startup read of the script file path; not turn-time model I/O"
)]
pub(crate) fn run_file(path: &str, script_args: Vec<String>, opts: BatchOpts) -> ExitCode {
    match std::fs::read_to_string(path) {
        Ok(source) => run_source(path, source, script_args, opts),
        Err(e) => {
            diagnostic::cmd_error("ral", &format!("{path}: {e}"));
            ExitCode::from(1)
        }
    }
}

/// Run the script arriving on stdin.
pub(crate) fn run_stdin(run: RunOpts) -> ExitCode {
    use std::io::Read as _;

    let mut source = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut source) {
        diagnostic::cmd_error("ral", &format!("<stdin>: {e}"));
        return ExitCode::from(1);
    }
    let batch = BatchOpts {
        run,
        ..BatchOpts::default()
    };
    run_source("<stdin>", source, Vec::new(), batch)
}

/// Serialise the run's report envelope to JSON and emit it on stderr — the
/// same envelope `audit { … }` returns, with the whole run as its body.  An
/// escape (`exit`) is not a failure: the process still exits with its code,
/// but the report reads `` `ok () ``.
fn emit_audit_report(ending: &Ending, trail: &[ral_core::serial::FOValue], pretty: bool) {
    use ral_core::types::{Value, report_value};
    let outcome = match ending {
        Ending::Settled { value, .. } => Ok(Value::from(value.clone())),
        Ending::Raised { record, .. }
        | Ending::Walled { record, .. }
        | Ending::Unreturnable { record, .. } => Err(Value::from(record.clone())),
        Ending::Exited(_) => Ok(Value::Unit),
    };
    let trail: Vec<Observation> = trail.iter().filter_map(Observation::from_wire).collect();
    let json_val = ral_core::builtins::value_to_json_lossy_bytes(&report_value(outcome, &trail));
    let json_str = if pretty {
        serde_json::to_string_pretty(&json_val).unwrap_or_default()
    } else {
        serde_json::to_string(&json_val).unwrap_or_default()
    };
    eprintln!("{json_str}");
}

/// Execute `source` non-interactively, under the name diagnostics will use
/// for it: a script path, `<stdin>`, or `-c`.
///
/// Boots a `batch` engine, dispatches its boot door, then the script; when
/// `--audit` is active, reports the script's run as one report envelope on
/// stderr. `--check` and `--dump-ast` are static, and boot nothing.
///
/// Every batch source passes through here, so line endings are normalised
/// here too — one door, one rule.
pub(crate) fn run_source(
    name: &str,
    source: String,
    script_args: Vec<String>,
    opts: BatchOpts,
) -> ExitCode {
    let source = ral_core::source::normalize_source_text(source);
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
    if check || dump_ast {
        return static_only(name, &source, dump_ast);
    }
    ral_core::process::install_handlers();
    // Seeds the ANSI color gate, so `_ansi-ok` and the prelude ansi-*
    // constants work in batch too.
    let (_, terminal) = probe_terminal(false);

    // RAL_TIMING is a presence probe, not a basedir.
    #[allow(clippy::disallowed_methods)]
    let timing = std::env::var_os("RAL_TIMING").is_some();
    let t0 = std::time::Instant::now();
    let tick = |label: &str| {
        if timing {
            eprintln!(
                "[timing] {label:12} {:.3}ms",
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }
    };

    let terminal_access = if terminal.startup_foreground {
        RequestedTerminalAccess::Leased
    } else {
        RequestedTerminalAccess::Denied
    };
    let attach = local_attach(BATCH, terminal, BatchConfig { args: script_args }.encode());
    let transport = match IdentityTransport::boot(&INSTALLERS, &attach) {
        Ok(transport) => transport,
        Err(severed) => {
            eprintln!("ral: {severed}");
            return ExitCode::from(2);
        }
    };
    let _signals = transport.control().forward_signals();
    transport.set_deferred_sink(Arc::new(Stdout));
    let run = |program, trail| Run {
        program,
        script_name: name.to_string(),
        caps: GrantStack::root(),
        wall: None,
        deferred_lease: None,
        worker_cap: None,
        io: RunIo::Inherit,
        terminal: terminal_access,
        stdin: RunStdin::Inherit,
        trail,
    };
    let boot = Boot {
        login: false,
        no_rc: true,
        recursion_limit,
        capabilities: capabilities
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
    };
    let booted = run(boot.program(), None);
    if let Err(status) = boot_door::settle(dispatch_to_report(&transport, booted, Arc::new(()))) {
        return ExitCode::from(exit_byte(status));
    }
    tick("boot");

    let script = run(
        Program::Source(source),
        audit.then_some(CapturePolicy::Bytes),
    );
    let status = dispatch(&transport, script, audit.then_some(pretty));
    tick("evaluate");
    ExitCode::from(exit_byte(status))
}

/// Dispatch `run` under the mute host and print what it rendered — or, under
/// `--audit` (`Some(pretty)`), its report envelope instead of a runtime error.
/// The run's status.
fn dispatch(transport: &IdentityTransport, run: Run, audit: Option<bool>) -> i32 {
    let report = match dispatch_to_report(transport, run, Arc::new(())) {
        Ok(report) => report,
        Err(severed) => {
            eprintln!("ral: {severed}");
            return 1;
        }
    };
    match report {
        Report::Static { rendered, status } => {
            eprint!("{rendered}");
            status
        }
        Report::Ran { ending, trail, .. } => {
            match (&ending, audit) {
                (_, Some(pretty)) => emit_audit_report(&ending, &trail, pretty),
                (
                    Ending::Raised { rendered, .. }
                    | Ending::Walled { rendered, .. }
                    | Ending::Unreturnable { rendered, .. },
                    None,
                ) => eprint!("{rendered}"),
                _ => {}
            }
            ending.status()
        }
    }
}

/// `--check` and `--dump-ast`: static work on the source alone.
fn static_only(name: &str, source: &str, dump_ast: bool) -> ExitCode {
    let parse_failed = |e| {
        eprint!(
            "{}",
            diagnostic::format_parse_error_ariadne(name, source, &e)
        );
        ExitCode::from(2)
    };
    let ast = match ral_core::syntax::parser::parse(source) {
        Ok(ast) => ast,
        Err(e) => return parse_failed(e),
    };
    if dump_ast {
        for node in &ast {
            eprintln!("{node:#?}");
        }
        return ExitCode::SUCCESS;
    }
    let top =
        match ral_core::elaborator::elaborate(&ast, std::collections::HashSet::default(), name) {
            Ok(top) => top,
            Err(e) => return parse_failed(e),
        };
    let schemes = ral_core::SessionSchemes::from_schemes(
        crate::PRELUDE.schemes(),
        batch_surface().builtin_table(),
    );
    match ral_core::typecheck(&top, schemes, None) {
        Ok(_) => ExitCode::SUCCESS,
        Err(errors) => {
            eprint!(
                "{}",
                diagnostic::format_type_errors_ariadne(name, source, &errors)
            );
            ExitCode::from(1)
        }
    }
}
