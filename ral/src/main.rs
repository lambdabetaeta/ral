//! Entry point for the `ral` interactive shell binary. [`startup`] decides
//! what kind of process this is; the rest of this file runs the shell when it
//! turns out to be one.

mod batch;
mod cli;
mod platform;
mod repl;
mod startup;

use cli::{InteractiveOpts, Mode};
use startup::Invocation;
use std::process::ExitCode;

/// The prelude baked into this binary at build time by `build.rs`.
pub(crate) static PRELUDE: ral_core::boot::BakedPrelude = ral_core::baked_prelude!();

fn main() -> ExitCode {
    startup::refuse_setuid();
    startup::adopt_process_dispositions();

    match startup::identify() {
        Invocation::Shell(mode) => run(mode),
        Invocation::Exit(code) => code,
    }
}

/// Run the session the arguments named.
fn run(mode: Mode) -> ExitCode {
    match mode {
        Mode::Login(opts) => interactive(true, opts),
        Mode::Interactive(opts) => interactive(false, opts),
        Mode::Script {
            path,
            script_args,
            batch,
        } => batch::run_file(&path, script_args, batch),
        Mode::Command {
            code,
            script_args,
            batch,
        } => batch::run_source("-c", code, script_args, batch),
    }
}

/// An interactive invocation whose stdin is a script is not interactive at
/// all: the script runs in batch, and no REPL is booted.
fn interactive(is_login: bool, opts: InteractiveOpts) -> ExitCode {
    if opts.reads_stdin_as_script() {
        batch::run_stdin(opts.run)
    } else {
        repl::run_interactive(is_login, &opts)
    }
}
