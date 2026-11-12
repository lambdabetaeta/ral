//! Non-interactive execution for script, stdin, and `-c` modes.

use ral_core::carrier::{IdentityTransport, Transport as _, dispatch_to_report};
use ral_core::data::FOValue;
use ral_core::data::datum::Datum as _;
use ral_core::own;
use ral_core::protocol::{Program, Ran, Report, Run};
use ral_core::shell::runtime::{CapturePolicy, DeferredSink, DoneEvent};
use ral_core::source::{SourceFile, SourceOrigin};
use ral_core::{RequestedTerminalAccess, io::terminal};
use ral_core::{err, errln, outln};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use crate::boot_builtin::{self, Boot};
use crate::cli::{BatchOpts, RunOpts};
use crate::platform::{local_attach, probe_terminal};
use crate::site::{Silent, site};
use crate::startup::engine::{BATCH, BatchConfig, INSTALLERS};

/// Batch's session surface: a watched worker's lines on stdout.
struct Stdout;

impl DeferredSink for Stdout {
    fn deliver(&self, batch: Vec<FOValue>) {
        for v in &batch {
            if let Some(line) = crate::surface::watch_line(v) {
                outln!("{line}");
            } else if DoneEvent::from_surface(v).is_some() {
                // A loose worker's ending is its own; batch announces none.
            } else if let Some(note) = crate::surface::dropped(v) {
                errln!("{note}");
            }
        }
    }
}

/// Run the script at `path`.
pub(crate) fn run_file(path: &Path, script_args: Vec<String>, opts: BatchOpts) -> ExitCode {
    let (real, source) = match site!(Silent::CliArgument, own::read_named_source(path)) {
        Ok(source) => source,
        Err(e) => {
            terminal::cmd_error("ral", &e.to_string());
            return ExitCode::from(1);
        }
    };
    let file = match SourceFile::new(real.as_path().to_path_buf()) {
        Ok(file) => file,
        Err(e) => {
            terminal::cmd_error("ral", &e.to_string());
            return ExitCode::from(1);
        }
    };
    run_source(&SourceOrigin::File(file), source, script_args, opts)
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

/// Emit an outcome and its unchanged wire trail as the `--audit` envelope.
fn emit_audit_report(outcome: FOValue, trail: Vec<FOValue>, pretty: bool) {
    let json_val = FOValue::Map {
        entries: vec![
            ("outcome".into(), outcome),
            ("trail".into(), FOValue::List { items: trail }),
        ],
    }
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
/// stderr. `--check` boots identically, then checks without evaluating.
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
        check,
        run: RunOpts {
            recursion_limit,
            capabilities,
        },
    } = opts;
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
        no_rc: true,
        recursion_limit,
        capabilities,
    };
    let booted = run(boot.program(), None);
    if let Err(status) =
        boot_builtin::settle(dispatch_to_report(&transport, booted, Arc::new(()), None))
    {
        return ExitCode::from(status);
    }
    tick("boot");

    let script = Run {
        last: true,
        ..run(
            if check {
                Program::Check(source)
            } else {
                Program::Source(source)
            },
            audit.then_some(CapturePolicy::Bytes),
        )
    };
    let status = dispatch(&transport, script, audit.then_some(pretty));
    tick(if check { "check" } else { "evaluate" });
    ExitCode::from(status)
}

/// Dispatch `run` under the mute host and print what it rendered — or, under
/// `--audit` (`Some(pretty)`), its report envelope instead of diagnostics.
/// The run's status.
fn dispatch(transport: &IdentityTransport, run: Run, audit: Option<bool>) -> u8 {
    let report = match dispatch_to_report(transport, run, Arc::new(()), None) {
        Ok(report) => report,
        Err(severed) => {
            errln!("ral: {severed}");
            return 1;
        }
    };
    let code = report.code();
    match report {
        Report::Static {
            rendered,
            status,
            trail,
        } => {
            if let Some(pretty) = audit {
                emit_audit_report(
                    FOValue::Variant {
                        label: "static".into(),
                        payload: Some(Box::new(FOValue::Map {
                            entries: vec![
                                ("rendered".into(), rendered.encode()),
                                ("status".into(), i64::from(status.get()).encode()),
                            ],
                        })),
                    },
                    trail,
                    pretty,
                );
            } else {
                err!("{rendered}");
            }
        }
        Report::Checked { trail } => {
            if let Some(pretty) = audit {
                emit_audit_report(
                    FOValue::Variant {
                        label: "checked".into(),
                        payload: None,
                    },
                    trail,
                    pretty,
                );
            }
        }
        Report::Faulted { rendered, .. } => err!("{rendered}"),
        Report::Ran(Ran {
            ending,
            rendered,
            trail,
            ..
        }) => {
            if let Some(pretty) = audit {
                emit_audit_report(ending.encode(), trail, pretty);
            } else if !rendered.is_empty() {
                err!("{rendered}");
            }
        }
    }
    code
}
