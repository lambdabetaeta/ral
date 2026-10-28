//! Interactive read-eval-print loop for the `ral` shell.
//!
//! Public surface is one function: [`run_interactive`].  The real work
//! lives in [`session::Session`], which boots an engine and drives the loop,
//! holding no `Shell`: it speaks only the protocol.
//!
//! Submodules factor out orthogonal concerns:
//! - [`completion`] -- Frontend-neutral completion engine (classification,
//!   candidate sources, prefix matching, fuzzy fallback) shared by every surface.
//! - [`complete`] -- The rustyline adapter over [`completion`]: completer,
//!   ghost-text hinter, and plugin syntax highlighting.
//! - [`config`]   -- RC file discovery, sourcing, and application, run
//!   engine-side inside the boot builtin.
//! - [`cursor`]   -- The zsh-style partial-line marker before each prompt.
//! - [`enquiry`]  -- The `repl-editor` and `repl-plugin` enquiry classes.
//! - [`errfmt`]   -- REPL-specific error formatting helpers.
//! - [`exec`]     -- One input line's dispatch and its lifecycle hooks.
//! - [`frontend`] -- The `Frontend` trait and its implementations.
//! - [`host`]     -- The REPL's `Host`: enquiries, surfaces, hook dispatch.
//! - [`keybinding`] -- Plugin keybinding dispatch.
//! - [`plugin`]   -- Plugin runtime state and hook machinery, host-side,
//!   plus the engine-side `_ed-*` builtins and load builtins.
//! - [`prompt`]   -- Prompt rendering.
//! - [`session`]  -- The REPL state machine driving the loop.
//! - [`theme`]    -- REPL value-output styling (configurable from rc).

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

pub(crate) use config::RcSettings;
pub(crate) use config::source::source_rc_file;
pub(crate) use frontend::Surface;
pub(crate) use plugin::ed_builtins::ED_BUILTINS;
pub(crate) use plugin::load::BUILTINS as PLUGIN_LOAD_BUILTINS;
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

/// Captured stdout as one line of terminal text: one trailing LF or CRLF
/// removed, the rest decoded lossily.
fn captured_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(ral_core::io::without_terminator(bytes)).into_owned()
}

/// Evaluate `src` on `shell` to its value.
#[cfg(test)]
pub(crate) fn eval(shell: &mut ral_core::Shell, src: &str) -> ral_core::Value {
    let run = exec::line_run(src);
    shell.run(run).expect_ran().into_result().expect("evaluate")
}

/// Core's surface with the REPL's prelude seated: the runtime a test engine
/// takes when its code reads prelude names.
#[cfg(test)]
pub(crate) fn prelude_runtime() -> ral_core::Runtime {
    ral_core::shell::boot::boot_runtime(
        ral_core::io::terminal::TerminalState::default(),
        &crate::PRELUDE,
        &ral_core::HostSurface::default(),
    )
    .expect("a test host resolves its baseline")
}

/// Boot an identity engine at `attach`, over a root on `runtime` that `dress`
/// fits out.
///
/// A recipe is a `fn`, so `runtime` and `dress` reach it through this thread:
/// `boot` runs the recipe on the calling one.
#[cfg(test)]
pub(crate) fn engine_at(
    attach: &ral_core::protocol::Attach,
    runtime: ral_core::Runtime,
    dress: impl FnOnce(&mut ral_core::Shell) + 'static,
) -> ral_core::carrier::IdentityTransport {
    use ral_core::engine::EngineInstaller;
    use std::cell::Cell;
    type Dress = Box<dyn FnOnce(&mut ral_core::Shell)>;
    thread_local! {
        static RUNTIME: Cell<Option<ral_core::Runtime>> = const { Cell::new(None) };
        static DRESS: Cell<Option<Dress>> = const { Cell::new(None) };
    }
    #[allow(
        clippy::unnecessary_wraps,
        reason = "must match EngineInstaller::runtime's signature, which can genuinely refuse"
    )]
    fn test_runtime(_: &ral_core::protocol::EngineSpec) -> Result<ral_core::Runtime, String> {
        Ok(RUNTIME
            .take()
            .expect("no runtime: was this test engine booted through `engine_at`?"))
    }
    #[allow(
        clippy::unnecessary_wraps,
        reason = "must match EngineInstaller::root's signature, which can genuinely refuse"
    )]
    fn dressed_root(
        _: &ral_core::protocol::Root,
        shell: &mut ral_core::Shell,
    ) -> Result<(), String> {
        if let Some(dress) = DRESS.take() {
            dress(shell);
        }
        Ok(())
    }
    static INSTALLERS: [EngineInstaller; 1] = [EngineInstaller {
        tag: TEST_TAG,
        runtime: test_runtime,
        root: dressed_root,
        narrow: |_, _| Err("a test engine makes no child sessions".into()),
    }];
    RUNTIME.set(Some(runtime));
    DRESS.set(Some(Box::new(dress)));
    ral_core::carrier::IdentityTransport::boot(&INSTALLERS, attach).expect("a test engine boots")
}

#[cfg(test)]
pub(crate) const TEST_TAG: &str = "test";

/// Dispatch `src` as an input line under the mute host.
#[cfg(test)]
pub(crate) fn run_line(t: &dyn ral_core::carrier::Transport, src: &str) {
    ral_core::carrier::dispatch_to_report(t, exec::line_run(src), std::sync::Arc::new(()), None)
        .expect("the engine is attached");
}

/// A test engine seated in the host's temp dir.
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
pub(crate) mod in_temp {
    /// [`engine_at`](super::engine_at), seated in the temp dir.
    pub(crate) fn engine(
        runtime: ral_core::Runtime,
        dress: impl FnOnce(&mut ral_core::Shell) + 'static,
    ) -> ral_core::carrier::IdentityTransport {
        let temp = std::env::temp_dir();
        super::engine_at(
            &ral_core::protocol::Attach::new(
                super::TEST_TAG,
                ral_core::path::EnginePath::Host(temp.clone()),
                ral_core::path::EnginePath::Host(temp),
            ),
            runtime,
            dress,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::captured_line;

    #[test]
    fn captured_line_removes_only_one_line_terminator() {
        for (bytes, expected) in [
            (b"x\r\n".as_slice(), "x"),
            (b"x\n", "x"),
            (b"x\r", "x\r"),
            (b"x\n\r\n", "x\n"),
            (b"x\r\ny\r\n", "x\r\ny"),
            (b"", ""),
            (b"\0\n", "\0"),
            (b"\xff\r\n", "\u{fffd}"),
        ] {
            assert_eq!(captured_line(bytes), expected);
        }
    }
}
