//! REPL session state machine.
//!
//! [`Session`] owns the long-lived state of an interactive shell — the
//! engine's transport, the REPL [`Host`](super::host::ReplHost), the
//! line-editing [`Frontend`], any pending buffer queued for re-edit, and the
//! exit code that will be returned to the OS. It holds no `Shell`.
//!
//! Process setup (signals, terminal claim, panic hook) and frontend
//! construction live in the [`boot`] submodule; this file holds the state
//! machine itself.

mod boot;

use ral_core::io::TerminalState;
use ral_core::protocol::{IdentityTransport, Transport, reading};
use ral_core::serial::datum::Datum as _;
use std::process::ExitCode;
use std::sync::Arc;

use super::exec::{Step, step};
use super::frontend::{EditBuffer, Frontend, Read};
use super::host::{ReplHost, hook_run, teardown_notice};
use super::prompt::{render as render_prompt, write_terminal_title};
use crate::boot_door::{self, Boot};
use crate::startup::engine::{INSTALLERS, REPL, ReplConfig};

/// Per-iteration loop control: stay in the loop, or break out and return
/// the recorded exit code.
pub(super) enum Flow {
    Continue,
    Break,
}

/// Long-lived interactive shell state.
///
/// Teardown (history flush, worker sweep) lives in [`Drop`] so it runs on
/// an unwinding panic too, not only on the orderly `run` exit — a crash
/// must not orphan a running worker or lose the session's history.
pub(super) struct Session {
    transport: Arc<IdentityTransport>,
    /// The process's signals, heard as the engine's `Control`.
    _signals: ral_core::process::AmbientForward,
    host: Arc<ReplHost>,
    frontend: Box<dyn Frontend>,
    terminal: TerminalState,
    /// Buffer the previous `read` asked us to re-feed (a plugin keybinding
    /// handler that returned [`Read::Edit`]).  The frontend may still drain
    /// its own internal stack when this is `None`.
    pending: Option<EditBuffer>,
    /// The reactive-worksheet model: per-binding dependency edges and the
    /// pure/effectful verdict, accumulated across runs and projected by the
    /// structural surface.  Owned here so it persists; recorded after a
    /// successful top-level bind and read by `frontend.read`.  Only the
    /// `structural` build constructs and reads it.
    #[cfg(feature = "structural")]
    worksheet: super::worksheet::Worksheet,
    /// Exit status to return when the loop ends.  Set by `exit` inside
    /// the evaluator; otherwise stays 0 on a clean EOF.
    exit_code: u8,
}

impl Session {
    /// Boot the `repl` engine, then dispatch its boot door and the rc's
    /// `startup` block. `Err(code)` is a boot that ended the session: a
    /// `--capabilities` failure, or an `exit` in a startup file.
    pub(super) fn boot(opts: &crate::cli::InteractiveOpts) -> Result<Self, ExitCode> {
        let exit = |status| ExitCode::from(crate::platform::exit_byte(status));
        boot::setup_signals();
        let (interactive_mode, terminal) = crate::platform::probe_terminal(true);
        boot::setup_panic_hook();

        let config = ReplConfig { login: opts.login }.encode();
        let attach = crate::platform::local_attach(REPL, terminal, config);
        let transport = Arc::new(IdentityTransport::boot(&INSTALLERS, &attach).map_err(
            |severed| {
                eprintln!("ral: {severed}");
                ExitCode::from(2)
            },
        )?);
        let signals = transport.control().forward_signals();
        let host = ReplHost::new(Arc::default());
        transport.set_deferred_sink(host.clone());

        let boot = Boot {
            login: opts.login,
            no_rc: opts.no_rc,
            recursion_limit: opts.run.recursion_limit,
            capabilities: opts
                .run
                .capabilities
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
        };
        let (report, _) = host.dispatch(&*transport, hook_run(boot.program(), None), None);
        let mut settings = boot_door::settle(report).map_err(exit)?;
        if let Some(surface) = opts.surface {
            settings.surface = surface;
        }
        super::theme::set_output_theme(settings.theme.clone());
        if settings.startup {
            let startup = ral_core::HookName::session("startup");
            if let Some(fault) = host
                .run_hook(&*transport, startup, vec![], None, None)
                .fault
            {
                eprintln!("{fault}");
            }
        }

        let mut frontend = boot::create_frontend(
            interactive_mode,
            &settings,
            transport.clone(),
            host.clone(),
            terminal,
        );
        host.set_printer(frontend.printer());

        Ok(Self {
            transport,
            _signals: signals,
            host,
            frontend,
            terminal,
            pending: None,
            #[cfg(feature = "structural")]
            worksheet: super::worksheet::Worksheet::default(),
            exit_code: 0,
        })
    }

    /// Drive the loop until a frontend reports EOF or `exit` escapes the
    /// evaluator.  History flush and the worker sweep happen in [`Drop`], so
    /// they cover a panic-unwind exit as well as this orderly one.
    pub(super) fn run(mut self) -> ExitCode {
        ral_core::dbg_trace!("repl", "entering REPL loop");
        while matches!(self.iterate(), Flow::Continue) {}
        ExitCode::from(self.exit_code)
    }

    /// Run one iteration: draw prompt, read, eval.  Returns `Break` when the
    /// frontend hits EOF, the evaluator returns an exit code, or the
    /// session has ended.
    fn iterate(&mut self) -> Flow {
        let t: &dyn Transport = &*self.transport;
        // A cancelled durable root ends the session.  Cancellation is one-way,
        // so after a SIGTERM/SIGHUP or a Ctrl-\ every future iteration would
        // fail with the same cause; exit with its code instead of dealing the
        // user an unusable prompt.
        if let Ok(Some(code)) = reading::session_ended(t) {
            self.exit_code = crate::platform::exit_byte(code);
            return Flow::Break;
        }

        // Acknowledge handled signals at the prompt boundary: the unwind
        // is done, and a stale escalation tick would otherwise creep the
        // next Ctrl-C toward the third-signal force-exit.
        ral_core::process::clear();
        let cwd = reading::cwd(t).unwrap_or_default();
        write_terminal_title(&self.terminal, &cwd.to_string_lossy());
        let prompt = render_prompt(t, &self.host);

        let read_result = self.frontend.read(
            t,
            &prompt,
            self.pending.take(),
            #[cfg(feature = "structural")]
            &self.worksheet,
        );
        match read_result {
            Read::Line(input) => {
                let trimmed = input.trim();
                if trimmed.is_empty() {
                    return Flow::Continue;
                }
                self.frontend.add_history(trimmed);
                self.eval(trimmed)
            }
            Read::Edit(buf) => {
                self.pending = Some(buf);
                Flow::Continue
            }
            Read::Interrupt => {
                ral_core::process::clear();
                self.transport.control().interrupt();
                Flow::Continue
            }
            Read::Eof => Flow::Break,
        }
    }

    /// Evaluate one non-empty trimmed input line, recording any exit
    /// code so [`run`](Self::run) can break cleanly.
    fn eval(&mut self, trimmed: &str) -> Flow {
        match step(
            trimmed,
            &*self.transport,
            &self.host,
            #[cfg(feature = "structural")]
            &mut self.worksheet,
        ) {
            Step::Continue => Flow::Continue,
            Step::Exit(c) => {
                self.exit_code = c;
                Flow::Break
            }
        }
    }
}

impl Drop for Session {
    /// Flush history and take down remaining workers.  Runs on both the
    /// orderly `run` return and a panic unwinding through the owned
    /// `Session`, so a crash mid-iteration does not lose the session's
    /// history.
    ///
    /// Name, then sweep: a still-running worker is announced here, once, and
    /// taken down — external children and all — when the transport's shell
    /// drops.  Naming never gates or delays the exit it announces.
    fn drop(&mut self) {
        let workers = reading::workers(&*self.transport).unwrap_or_default();
        self.transport.detach();
        self.frontend.save_history();
        if let Some(notice) = teardown_notice(&workers) {
            eprintln!("{notice}");
        }
        // Windows-only, no-op elsewhere: reverts this session's AppContainer
        // grant ACEs and deletes its profile.
        ral_core::sandbox::teardown_session();
    }
}
