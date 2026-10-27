//! Entry point for the `ral` interactive shell binary. [`startup`] decides
//! what kind of process this is; the rest of this file runs the shell when it
//! turns out to be one.

mod batch;
mod boot_builtin;
#[cfg(unix)]
mod bridge;
mod cli;
#[cfg(unix)]
mod login;
mod platform;
mod repl;
mod site;
mod startup;
mod surface;

use cli::{InteractiveOpts, Mode};
use startup::Invocation;
use std::process::ExitCode;

/// The prelude baked into this binary at build time by `build.rs`.
pub(crate) static PRELUDE: ral_core::shell::boot::BakedPrelude = ral_core::baked_prelude!();

/// musl's allocator slows the bundled `rg`; ripgrep's own remedy (jemalloc lacks i686).
#[cfg(all(feature = "ripgrep", target_env = "musl", target_pointer_width = "64"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() -> ExitCode {
    startup::refuse_setuid();
    #[cfg(unix)]
    if let Some(code) = bridge::serve() {
        return code;
    }
    startup::adopt_process_dispositions();

    match startup::identify() {
        Invocation::Shell(mode) => run(mode),
        Invocation::Exit(code) => code,
    }
}

/// Run the session the arguments named.
fn run(mode: Mode) -> ExitCode {
    #[cfg(unix)]
    if let Mode::Interactive(opts) = &mode
        && opts.adopts_login_env()
    {
        // SAFETY: refuse_setuid, bridge, dispositions and identify spawn no threads.
        // identify only pins; Session::boot installs signals and boots the engine later.
        unsafe { login::adopt_posix_profile() };
    }
    startup::warn_unreadable_env();
    match mode {
        Mode::Interactive(opts) => interactive(opts),
        Mode::Script {
            path,
            script_args,
            batch,
        } => batch::run_file(&path, script_args, batch),
        Mode::Command {
            code,
            script_args,
            batch,
        } => batch::run_source(
            &ral_core::source::SourceOrigin::named("-c"),
            code,
            script_args,
            batch,
        ),
    }
}

/// An interactive invocation whose stdin is a script is not interactive at
/// all: the script runs in batch, and no REPL is booted.
fn interactive(opts: InteractiveOpts) -> ExitCode {
    if opts.reads_stdin_as_script() {
        batch::run_stdin(opts.run)
    } else {
        repl::run_interactive(&opts)
    }
}
