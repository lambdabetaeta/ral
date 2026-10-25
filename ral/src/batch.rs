//! Non-interactive execution for script, stdin, and `-c` modes.

use ral_core::carrier::{IdentityTransport, Transport as _, dispatch_to_report};
use ral_core::compile::{CompileError, compile_and_typecheck, on_compile_stack};
use ral_core::data::FOValue;
use ral_core::data::datum::Datum as _;
use ral_core::elaborator::elaborate;
use ral_core::own;
use ral_core::protocol::{Ending, Program, Report, Run};
use ral_core::shell::run::StaticDiagnostics;
use ral_core::shell::runtime::{CapturePolicy, DeferredSink, Observation};
use ral_core::source::{FileId, Source, SourceFile, SourceOrigin};
use ral_core::syntax::parser::parse;
use ral_core::{RequestedTerminalAccess, SessionSchemes, io::terminal};
use ral_core::{err, errln, outln};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use crate::boot_builtin::{self, Boot};
use crate::cli::{BatchOpts, Halt, RunOpts};
use crate::platform::{exit_byte, local_attach, probe_terminal};
use crate::site::{Silent, site};
use crate::startup::engine::{BATCH, BatchConfig, INSTALLERS, batch_surface};

/// Batch's session surface: a watched worker's lines on stdout.
struct Stdout;

impl DeferredSink for Stdout {
    fn deliver(&self, batch: Vec<FOValue>) {
        for v in &batch {
            if let Some(line) = crate::surface::watch_line(v) {
                outln!("{line}");
            } else if let Some(note) = crate::surface::dropped(v) {
                errln!("{note}");
            }
        }
    }
}

/// Run the script at `path`.
pub(crate) fn run_file(path: &Path, script_args: Vec<String>, opts: BatchOpts) -> ExitCode {
    let file = match SourceFile::new(path) {
        Ok(file) => file,
        Err(e) => {
            terminal::cmd_error("ral", &e.to_string());
            return ExitCode::from(1);
        }
    };
    match site!(Silent::CliArgument, own::read_named(path)) {
        Ok(source) => run_source(&SourceOrigin::File(file), source, script_args, opts),
        Err(e) => {
            terminal::cmd_error("ral", &e.to_string());
            ExitCode::from(1)
        }
    }
}

/// Run the script arriving on stdin.
pub(crate) fn run_stdin(run: RunOpts) -> ExitCode {
    use std::io::Read as _;

    let mut source = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut source) {
        terminal::cmd_error("ral", &format!("<stdin>: {e}"));
        return ExitCode::from(1);
    }
    let batch = BatchOpts {
        run,
        ..BatchOpts::default()
    };
    run_source(&SourceOrigin::named("<stdin>"), source, Vec::new(), batch)
}

/// Serialise the run's report envelope to JSON and emit it on stderr — the
/// same envelope `audit { … }` returns, with the whole run as its body.  An
/// escape (`exit`) is not a failure: the process still exits with its code,
/// but the report reads `` `ok () ``; a stop reads `` `stopped cause ``.
fn emit_audit_report(ending: &Ending, trail: &[ral_core::data::FOValue], pretty: bool) {
    use ral_core::data::datum::Datum as _;
    use ral_core::shell::runtime::{ErrorRecord, Status, Value, outcome_value, report_envelope};
    let outcome = match ending {
        Ending::Settled { value, .. } => outcome_value(Ok(Value::from(value.clone()))),
        Ending::Raised { record, .. } | Ending::Unreturnable { record, .. } => {
            outcome_value(Err(ErrorRecord::decode(record).unwrap_or_else(|why| {
                ErrorRecord::new("<runtime>", &Status::Raised(1), &why, None)
            })))
        }
        Ending::Exited(_) => outcome_value(Ok(Value::Unit)),
        Ending::Stopped { cause, .. } => Value::variant("stopped", Some(Value::from_datum(*cause))),
    };
    let trail: Vec<Observation> = trail
        .iter()
        .filter_map(|fo| Observation::decode(fo).ok())
        .collect();
    let json_val = FOValue::try_from(&report_envelope(outcome, trail))
        .expect("a report is built from first-order values")
        .to_json(|b| serde_json::Value::String(String::from_utf8_lossy(b).into_owned()));
    let json_str = if pretty {
        serde_json::to_string_pretty(&json_val).unwrap_or_default()
    } else {
        serde_json::to_string(&json_val).unwrap_or_default()
    };
    errln!("{json_str}");
}

/// Execute `source`, read from `origin`, non-interactively: a script file, or
/// the virtual `<stdin>` or `-c`.
///
/// Boots a `batch` engine, dispatches its boot builtin, then the script; when
/// `--audit` is active, reports the script's run as one report envelope on
/// stderr. `--check`, `--dump-ast` and `--dump-ir` are static, and boot nothing.
///
/// Every batch source passes through here, so line endings are normalised
/// here too — one place, one rule.
pub(crate) fn run_source(
    origin: &SourceOrigin,
    source: String,
    script_args: Vec<String>,
    opts: BatchOpts,
) -> ExitCode {
    let source = ral_core::source::normalize_source_text(source);
    let BatchOpts {
        audit,
        pretty,
        halt,
        run: RunOpts {
            recursion_limit,
            capabilities,
        },
    } = opts;
    if let Some(halt) = halt {
        return halted(halt, origin, &source);
    }
    let capabilities = match Boot::profiles(&capabilities) {
        Ok(capabilities) => capabilities,
        Err(e) => {
            errln!("{e}");
            return ExitCode::from(2);
        }
    };
    ral_core::process::install_handlers();
    // Seeds the ANSI color gate, so `_ansi-ok` and the prelude ansi-*
    // constants work in batch too.
    let (_, terminal) = probe_terminal(false);

    let timing = ral_core::host::var_os("RAL_TIMING").is_some();
    let t0 = std::time::Instant::now();
    let tick = |label: &str| {
        if timing {
            errln!(
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
    let attach = match local_attach(BATCH, terminal, BatchConfig { args: script_args }.encode()) {
        Ok(attach) => attach,
        Err(why) => {
            errln!("ral: {why}");
            return ExitCode::from(2);
        }
    };
    let transport = match IdentityTransport::boot(&INSTALLERS, &attach) {
        Ok(transport) => transport,
        Err(severed) => {
            errln!("ral: {severed}");
            return ExitCode::from(2);
        }
    };
    let _signals = transport.control().forward_signals();
    transport.set_deferred_sink(Arc::new(Stdout));
    let run = |program, trail| Run {
        terminal: terminal_access,
        trail,
        ..Run::foreground(program, origin.clone())
    };
    let boot = Boot {
        login: false,
        no_rc: true,
        recursion_limit,
        capabilities,
    };
    let booted = run(boot.program(), None);
    if let Err(status) = boot_builtin::settle(dispatch_to_report(&transport, booted, Arc::new(())))
    {
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
            errln!("ral: {severed}");
            return 1;
        }
    };
    match report {
        Report::Static { rendered, status } | Report::Faulted { rendered, status } => {
            err!("{rendered}");
            status.get()
        }
        Report::Ran { ending, trail, .. } => {
            match (&ending, audit) {
                (_, Some(pretty)) => emit_audit_report(&ending, &trail, pretty),
                (
                    Ending::Raised { rendered, .. }
                    | Ending::Stopped { rendered, .. }
                    | Ending::Unreturnable { rendered, .. },
                    None,
                ) => err!("{rendered}"),
                _ => {}
            }
            ending.status()
        }
    }
}

/// `--check`, `--dump-ast` and `--dump-ir`: static work on the source alone.
fn halted(halt: Halt, origin: &SourceOrigin, source: &str) -> ExitCode {
    let outcome = match halt {
        Halt::Ast => on_compile_stack(|| {
            parse(source)
                .map(|ast| ast.iter().for_each(|node| errln!("{node:#?}")))
                .map_err(CompileError::Parse)
        }),
        Halt::Ir => on_compile_stack(|| {
            parse(source)
                .and_then(|ast| elaborate(&ast, [], origin.file()))
                .map(|phrases| errln!("{phrases:#?}"))
                .map_err(CompileError::Parse)
        }),
        Halt::Checked => {
            let schemes =
                SessionSchemes::from_prelude(crate::PRELUDE.comp(), batch_surface().manifest());
            compile_and_typecheck(source, schemes, FileId::DUMMY, origin.file(), None).map(drop)
        }
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let (rendered, status) =
                StaticDiagnostics::Compile(e.reject(Source::from_text(origin.clone(), source)))
                    .render();
            err!("{rendered}");
            ExitCode::from(exit_byte(status))
        }
    }
}
