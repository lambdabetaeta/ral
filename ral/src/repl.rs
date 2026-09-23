//! Interactive read-eval-print loop for the `ral` shell.
//!
//! Public surface is one function: [`run_interactive`].  The real work
//! lives in [`session::Session`], which boots an engine and drives the loop,
//! holding no `Shell`: it speaks only the protocol.
//!
//! Submodules factor out orthogonal concerns:
//! - [`completion`] -- Frontend-neutral completion engine (classification,
//!   candidate sources, fuzzy/prefix ranking) shared by every surface.
//! - [`complete`] -- The rustyline adapter over [`completion`]: completer,
//!   ghost-text hinter, and plugin syntax highlighting.
//! - [`config`]   -- RC file discovery, sourcing, and application, run
//!   engine-side inside the boot door.
//! - [`cursor`]   -- ANSI cursor-position queries (Unix only).
//! - [`enquiry`]  -- The `repl-editor` and `repl-plugin` enquiry classes.
//! - [`errfmt`]   -- REPL-specific error formatting helpers.
//! - [`exec`]     -- One input line's dispatch and its lifecycle hooks.
//! - [`frontend`] -- The `Frontend` trait and its implementations.
//! - [`host`]     -- The REPL's `Host`: enquiries, surfaces, hook dispatch.
//! - [`keybinding`] -- Plugin keybinding dispatch.
//! - [`plugin`]   -- Plugin runtime state and hook machinery, host-side,
//!   plus the engine-side `_ed-*` builtins and load doors.
//! - [`prompt`]   -- Prompt rendering.
//! - [`session`]  -- The REPL state machine driving the loop.
//! - [`theme`]    -- REPL value-output styling (configurable from rc).
//! - [`worksheet`] -- The REPL-side worksheet model: per-binding
//!   dependency edges and the pure/effectful verdict, retained across runs
//!   for the structural surface's reactive worksheet.

mod complete;
mod completion;
mod config;
mod plugin;

mod cursor;
mod enquiry;
mod errfmt;
mod exec;
mod frontend;
mod highlight_style;
mod host;
mod keybinding;
mod prompt;
mod session;
mod theme;
#[cfg(feature = "structural")]
mod worksheet;

pub(crate) use config::RcSettings;
pub(crate) use config::source::source_startup_files;
pub(crate) use frontend::Surface;
pub(crate) use plugin::ed_builtins::ED_BUILTINS;
pub(crate) use plugin::load::DOORS as PLUGIN_DOORS;
pub(crate) use prompt::install_default_prompt;
use session::Session;
use std::process::ExitCode;

/// Enter the interactive REPL, returning the exit code to hand the OS.
pub(crate) fn run_interactive(opts: &crate::cli::InteractiveOpts) -> ExitCode {
    match Session::boot(opts) {
        Ok(session) => session.run(),
        Err(code) => code,
    }
}

/// Evaluate `src` on `shell` to its value.
#[cfg(test)]
pub(crate) fn eval(shell: &mut ral_core::Shell, src: &str) -> ral_core::Value {
    let run = exec::line_run(src);
    match shell.run(ral_core::RunRequest {
        run,
        surface: None,
        deferred: None,
        desk: None,
        fork: None,
    }) {
        ral_core::RunReport::Ran { ending, .. } => ending.into_result().expect("evaluate"),
        ral_core::RunReport::Static { .. } => panic!("well-formed source must run: {src:?}"),
    }
}

/// Boot an identity engine at `attach`, over a fresh shell `dress` fits out.
///
/// A recipe is a `fn`, so `dress` reaches it through this thread: `boot` runs
/// the recipe on the calling one.
#[cfg(test)]
pub(crate) fn engine_at(
    attach: &ral_core::protocol::Attach,
    dress: impl FnOnce(&mut ral_core::Shell) + 'static,
) -> ral_core::protocol::IdentityTransport {
    use ral_core::engine::{Booted, EngineInstaller};
    type Dress = Box<dyn FnOnce(&mut ral_core::Shell)>;
    thread_local! {
        static DRESS: std::cell::Cell<Option<Dress>> = const { std::cell::Cell::new(None) };
    }
    #[allow(
        clippy::unnecessary_wraps,
        reason = "must match EngineInstaller::boot's signature, which can genuinely refuse"
    )]
    fn dressed(_: &ral_core::protocol::Attach) -> Result<Booted, String> {
        let mut shell = ral_core::Shell::new(ral_core::io::TerminalState::default());
        if let Some(dress) = DRESS.take() {
            dress(&mut shell);
        }
        Ok(Booted {
            shell,
            keep: Box::new(()),
        })
    }
    static INSTALLERS: [EngineInstaller; 1] = [EngineInstaller {
        tag: TEST_TAG,
        boot: dressed,
        narrow: |_, _| Err("a test engine hatches no children".into()),
    }];
    DRESS.set(Some(Box::new(dress)));
    ral_core::protocol::IdentityTransport::boot(&INSTALLERS, attach).expect("a test engine boots")
}

#[cfg(test)]
pub(crate) const TEST_TAG: &str = "test";

/// [`engine_at`], seated in the temp dir.
#[cfg(test)]
pub(crate) fn engine(
    dress: impl FnOnce(&mut ral_core::Shell) + 'static,
) -> ral_core::protocol::IdentityTransport {
    let temp = std::env::temp_dir();
    engine_at(
        &ral_core::protocol::Attach::new(TEST_TAG, temp.clone(), temp),
        dress,
    )
}

/// Dispatch `src` as an input line under the mute host.
#[cfg(test)]
pub(crate) fn run_line(t: &dyn ral_core::protocol::Transport, src: &str) {
    ral_core::protocol::dispatch_to_report(t, exec::line_run(src), std::sync::Arc::new(()))
        .expect("the engine is attached");
}
