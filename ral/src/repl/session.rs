//! REPL session state machine.
//!
//! [`Session`] owns the long-lived state of an interactive shell — the
//! evaluator [`Shell`](ral_core::Shell), the [`JobTable`](crate::jobs::JobTable), the
//! line-editing [`Frontend`], any pending buffer queued for re-edit, and
//! the exit code that will be returned to the OS.
//!
//! Bootstrap (signals, terminal probe, builtins, profile/RC sourcing,
//! capability narrowing, frontend construction) lives in the [`boot`]
//! submodule; this file holds the state machine itself.

mod boot;

use ral_core::transport::Transport;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use super::exec::{Step, step};
use super::frontend::{EditBuffer, Frontend, Read};
use super::plugin::PluginRuntime;
use super::prompt::{render as render_prompt, write_terminal_title};

use crate::jobs;

/// Per-iteration loop control: stay in the loop, or break out and return
/// the recorded exit code.
pub(super) enum Flow {
    Continue,
    Break,
}

/// Long-lived interactive shell state.
///
/// Teardown (history flush, job reaping) lives in [`Drop`] so it runs on
/// an unwinding panic too, not only on the orderly `run` exit — a crash
/// must not orphan a stopped process group or lose the session's history.
pub(super) struct Session {
    transport: ral_core::transport::IdentityTransport,
    /// Job table shared with captured builtins installed at boot so
    /// `jobs`, `fg`, `bg`, and `disown` can mutate it from closures.
    jobs: Arc<Mutex<jobs::JobTable>>,
    frontend: Box<dyn Frontend>,
    /// Plugin runtime: lives here so REPL pre-eval handlers, lifecycle-
    /// hook fold, and the prompt fold can all reach it without
    /// threading it through the frontend.
    runtime: Arc<Mutex<PluginRuntime>>,
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
    /// One-shot setup: signals, terminal, builtins, profiles, capabilities,
    /// frontend.  Returns `Err(code)` only from `--capabilities`: a load
    /// failure or an escape raised while a capabilities profile evaluates.
    /// Profile/rc errors and escapes are reported and tolerated — a broken
    /// startup file must not strand the user, so the sourcing helpers in
    /// `boot` report and continue.
    pub(super) fn boot(
        is_login: bool,
        opts: &crate::cli::InteractiveOpts,
    ) -> Result<Self, ExitCode> {
        boot::setup_signals();
        let (interactive_mode, terminal) = crate::platform::probe_terminal(true);
        let jobs = Arc::new(Mutex::new(jobs::JobTable::new()));
        let runtime = Arc::new(Mutex::new(PluginRuntime::default()));
        // The host surface — the editor (`_ed-*`) builtins, `watch`, and the
        // captured job-control/plugin-lifecycle commands — rides the boot:
        // the typechecker reads this shell's builtin table, and plugins
        // loaded from rc are checked against it.
        let mut shell = ral_core::boot::boot_shell(
            terminal,
            &crate::PRELUDE,
            &ral_core::HostSurface {
                statics: vec![
                    super::plugin::ed_builtins::ED_BUILTINS,
                    ral_core::builtins::WATCH_BUILTIN,
                ],
                captured: vec![super::host_handlers::build(jobs.clone(), runtime.clone())],
            },
        );
        shell.set_exit_hints(crate::platform::load_exit_hints());
        // The REPL owns this process's signals: Ctrl-C reaches its runs and
        // Ctrl-\ / SIGTERM its durable root.
        shell.face_signals();
        boot::setup_panic_hook();

        // Login shell: set umask and source system/user profiles.
        #[cfg(unix)]
        if is_login {
            rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o022));
        }

        boot::setup_terminal(&mut shell);
        let mut rc = boot::load_profiles(is_login, opts.no_rc, &mut shell, &runtime);
        // CLI flags win over rc — apply after load_profiles.
        if let Some(n) = opts.run.recursion_limit {
            shell.set_stack_limit(n);
        }
        if let Some(s) = opts.surface {
            rc.surface = s;
        }
        // `--capabilities` applies after rc files: rc is operator-trusted
        // session bootstrap, the user-supplied ceiling narrows from there.
        crate::platform::apply_session_capabilities(&mut shell, &opts.run.capabilities)?;

        // Install the default prompt only when rc files did not
        // register one already (e.g. via the `prompt` key in ralrc).
        boot::install_default_prompt(&mut shell);

        let frontend = boot::create_frontend(interactive_mode, rc, &mut shell, runtime.clone());

        // Prompt rendering, the terminal title, and `frontend.read()` all reach
        // through `shell_mut()`, which only `IdentityTransport` offers.
        let transport = ral_core::transport::IdentityTransport::new(shell);

        Ok(Self {
            transport,
            jobs,
            frontend,
            runtime,
            pending: None,
            #[cfg(feature = "structural")]
            worksheet: super::worksheet::Worksheet::default(),
            exit_code: 0,
        })
    }

    /// Drive the loop until a frontend reports EOF or `exit` escapes the
    /// evaluator.  History flush and job reaping happen in [`Drop`], so
    /// they cover a panic-unwind exit as well as this orderly one.
    pub(super) fn run(mut self) -> ExitCode {
        ral_core::dbg_trace!("repl", "entering REPL loop");
        // No `attach` here: under `IdentityTransport` the shell is already
        // booted and dressed by `boot`, and its `attach` discards everything
        // but the terminal lease (hardcoded `None`).  Computing cwd/HOME/a
        // re-probed terminal only to throw them away is dead work — wire it
        // when `WireTransport` is actually constructed.
        while matches!(self.iterate(), Flow::Continue) {}
        ExitCode::from(self.exit_code)
    }

    /// Run one iteration: reap children, draw prompt, read, eval.
    /// Returns `Break` when the frontend hits EOF, the evaluator
    /// returns an exit code, or the session's durable root has been
    /// cancelled.
    fn iterate(&mut self) -> Flow {
        self.jobs.lock().unwrap().reap();

        // A cancelled durable root ends the session.  Cancellation is
        // one-way — the root can never be un-cancelled — so after a
        // SIGTERM/SIGHUP (`Terminate`) or a Ctrl-\ (`RootAbort`) every
        // future iteration would fail with the same cause; exit with its
        // code instead of dealing the user an unusable prompt.
        let cancel_cause = self
            .transport
            .shell_mut()
            .shell
            .cancel_handle()
            .as_scope()
            .cause();
        if let Some(cause) = cancel_cause {
            self.exit_code = crate::platform::exit_byte(cause.exit_code());
            return Flow::Break;
        }

        // Acknowledge handled signals at the prompt boundary: the unwind
        // is done, and a stale escalation tick would otherwise creep the
        // next Ctrl-C toward the third-signal force-exit.
        ral_core::process::clear();
        write_terminal_title(&self.transport.shell_mut().shell);
        let prompt = render_prompt(&mut self.transport.shell_mut().shell, &self.runtime);

        let read_result = {
            let mut guard = self.transport.shell_mut();
            self.frontend.read(
                &mut guard.shell,
                &prompt,
                self.pending.take(),
                #[cfg(unix)]
                &self.jobs,
                #[cfg(feature = "structural")]
                &self.worksheet,
            )
        };
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
                ral_core::transport::ControlSender::cancel_current();
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
            &self.transport,
            #[cfg(unix)]
            &self.jobs,
            &self.runtime,
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
    /// Flush history and take down remaining jobs.  Runs on both the
    /// orderly `run` return and a panic unwinding through the owned
    /// `Session`, so a crash mid-iteration neither orphans a stopped process
    /// group nor drops the session's history.
    ///
    /// Name, then sweep: a still-running worker is announced here, once, and
    /// taken down — external children and all — when the transport's shell
    /// drops.  Naming never gates or delays the exit it announces.
    fn drop(&mut self) {
        self.transport.detach();
        self.frontend.save_history();
        let workers = self.transport.shell_mut().shell.workers();
        if let Some(notice) = super::host_handlers::teardown_notice(&workers) {
            eprintln!("{notice}");
        }
        // A panic that poisons the JobTable still leaves it structurally
        // valid for a best-effort SIGTERM/SIGKILL sweep; recover the guard
        // rather than re-panicking into a process abort during unwind.
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cleanup();
        // Windows-only, no-op elsewhere: reverts this session's AppContainer
        // grant ACEs and deletes its profile.
        ral_core::sandbox::teardown_session();
    }
}
