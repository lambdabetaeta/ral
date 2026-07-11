//! Entry point for the `ral` interactive shell binary.
//!
//! This crate root handles process-level setup, bakes the prelude, registers
//! host services, and dispatches into either the REPL or the batch runner.

use clap::Parser as _;
use ral_core::diagnostic;
use std::process::ExitCode;

mod batch;
mod cli;
mod jobs;
mod platform;
mod repl;

use batch::run_batch;
use cli::{BatchOpts, Cli, InteractiveOpts, Mode, inject_arg_terminator};

/// The prelude baked into this binary at build time by `build.rs`.
pub(crate) static PRELUDE: ral_core::driver::BakedPrelude = ral_core::baked_prelude!();

/// The tag the REPL's `Transport::attach` names as its builtin installer.
/// The REPL captures its host builtins (`jobs`/`fg`/`bg`/…) as boot-time
/// closures over co-resident state (`repl::host_handlers`), which a wire
/// engine child cannot construct — so this tag maps to "install nothing",
/// the honest absence the bare REPL already gives every other host facility.
pub(crate) const ENGINE_INSTALLER_TAG: &str = "repl";

fn main() -> ExitCode {
    #[cfg(windows)]
    ral_core::io::enable_virtual_terminal_processing();

    // Restore SIGPIPE to SIG_DFL once at startup so in-process uutils and
    // pipeline helpers see the default disposition.
    #[cfg(unix)]
    ral_core::builtins::uutils::init_signal_dispositions();

    #[cfg(unix)]
    if std::env::args().any(|a| a == "--engine") {
        ral_core::engine::run_engine(&[ral_core::engine::EngineInstaller {
            tag: ENGINE_INSTALLER_TAG,
            prelude: &PRELUDE,
            install: |_shell| {},
        }]);
    }

    if let Some(code) = ral_core::try_run_pipeline_stage_helper() {
        return ExitCode::from(code);
    }
    if let Some(code) = ral_core::test_helper::try_run_test_helper() {
        return ExitCode::from(code);
    }

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (stripped, exit) = match ral_core::sandbox::early_init(&argv) {
        Ok(result) => result,
        Err(e) => {
            diagnostic::cmd_error("ral", &e);
            return ExitCode::from(1);
        }
    };
    if let Some(code) = exit {
        return ExitCode::from(code);
    }

    // Per-command sandbox re-exec tails run after `early_init`: a
    // `--sandbox-projection` child enters the OS sandbox first, then runs the
    // target confined. Both tails exit here, never reaching clap.
    if let Some(code) = ral_core::sandbox::serve_sandbox_exec(&stripped) {
        return ExitCode::from(code);
    }
    if let Some(code) = ral_core::try_run_bundled_tool(&stripped) {
        return ExitCode::from(code);
    }

    let mode = parse_mode(&stripped);

    // Refuse to run setuid: the shell inherits the caller's environment and
    // must not run with elevated privileges the user did not request.
    #[cfg(unix)]
    unsafe {
        if libc::geteuid() != libc::getuid() {
            eprintln!("ral: refusing to run setuid");
            return ExitCode::from(1);
        }
    }

    ral_core::builtins::help::register_prelude_type_hints(PRELUDE.schemes());

    // Publish the ral host surface (`_ed-*` editor builtins and `watch`) before
    // any builtin lookup runs. All modes go through builtin dispatch and type
    // seeding, so registration is unconditional.
    repl::register_host_surface();

    run_mode(mode)
}

fn parse_mode(args: &[String]) -> Mode {
    Cli::parse_from(std::iter::once("ral".to_string()).chain(inject_arg_terminator(args)))
        .into_mode()
}

fn run_mode(mode: Mode) -> ExitCode {
    match mode {
        Mode::Login(opts) => run_interactive(true, opts),
        Mode::Interactive(opts) => run_interactive(false, opts),
        Mode::Script {
            path,
            script_args,
            batch,
        } => run_script(&path, script_args, batch),
        Mode::Command {
            code,
            script_args,
            batch,
        } => run_batch("-c", &code, script_args, batch),
    }
}

fn run_interactive(is_login: bool, opts: InteractiveOpts) -> ExitCode {
    if opts.reads_stdin_as_script() {
        run_stdin_script(opts)
    } else {
        repl::run_interactive(is_login, &opts)
    }
}

fn run_stdin_script(opts: InteractiveOpts) -> ExitCode {
    use std::io::Read as _;

    let mut source = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut source) {
        diagnostic::cmd_error("ral", &format!("<stdin>: {e}"));
        return ExitCode::from(1);
    }
    let source = ral_core::source::normalize_source_text(source);
    run_batch(
        "<stdin>",
        &source,
        Vec::new(),
        BatchOpts {
            run: opts.run,
            ..BatchOpts::default()
        },
    )
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:script-read] startup read of the script file path; not turn-time model I/O"
)]
fn run_script(path: &str, script_args: Vec<String>, batch: BatchOpts) -> ExitCode {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            diagnostic::cmd_error("ral", &format!("{path}: {e}"));
            return ExitCode::from(1);
        }
    };
    let source = ral_core::source::normalize_source_text(source);
    run_batch(path, &source, script_args, batch)
}
