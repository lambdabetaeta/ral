//! REPL session state machine.
//!
//! [`Session`] owns the long-lived state of an interactive shell — the
//! evaluator [`Shell`], the [`JobTable`](crate::jobs::JobTable), the
//! line-editing [`Frontend`], any pending buffer queued for re-edit, and
//! the exit code that will be returned to the OS.
//!
//! Bootstrap (signals, terminal probe, builtins, profile/RC sourcing,
//! capability narrowing, frontend construction) lives in the [`boot`]
//! submodule; this file holds the state machine itself.

mod boot;

use ral_core::Shell;
use rustyline::config::{BellStyle, EditMode};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use super::config::RcCtx;
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
    shell: Shell,
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
    /// Exit status to return when the loop ends.  Set by `exit` inside
    /// the evaluator; otherwise stays 0 on a clean EOF.
    exit_code: u8,
}

impl Session {
    /// One-shot setup: signals, terminal, builtins, profiles, capabilities,
    /// frontend.  Returns `Err(code)` only from `--capabilities`: a load
    /// failure or an escape raised while a capabilities profile evaluates.
    /// Profile/rc errors and escapes are reported and tolerated — a broken
    /// rc must not strand the user, so `source_config_inner` swallows them.
    pub(super) fn boot(is_login: bool, opts: &crate::InteractiveOpts) -> Result<Self, ExitCode> {
        boot::setup_signals();
        let (interactive_mode, terminal) = crate::probe_terminal(true);
        ral_core::dbg_trace!("repl", "before register");
        let mut shell = ral_core::host::boot_shell(terminal, &crate::PRELUDE);
        ral_core::dbg_trace!("repl", "after register");
        shell.set_exit_hints(crate::load_exit_hints());
        boot::setup_panic_hook();

        // Login shell: set umask and source system/user profiles.
        #[cfg(unix)]
        if is_login {
            unsafe {
                libc::umask(0o022);
            }
        }

        boot::setup_terminal(&mut shell);
        let mut edit_mode = EditMode::Emacs;
        let mut bell = BellStyle::None;
        boot::install_default_prompt(&mut shell);

        let jobs = Arc::new(Mutex::new(jobs::JobTable::new()));
        let runtime = Arc::new(Mutex::new(PluginRuntime::default()));

        boot::load_profiles(
            is_login,
            opts.no_rc,
            &mut RcCtx {
                shell: &mut shell,
                edit_mode: &mut edit_mode,
                bell: &mut bell,
                runtime: &runtime,
            },
        );
        // CLI flag wins over rc — apply after load_profiles.
        if let Some(n) = opts.run.recursion_limit {
            shell.mobile.control.recursion_limit = n;
        }
        // `--capabilities` applies after rc files: rc is operator-trusted
        // session bootstrap, the user-supplied ceiling narrows from there.
        crate::apply_session_capabilities(&mut shell, &opts.run.capabilities)?;

        // Install the host surface — the editor (`_ed-*`) builtins and
        // `watch` — into this shell's builtin table.  The process registry
        // already learned about both through `register_host_surface()` at
        // process start, so the typechecker can see their schemes.
        shell.install_builtins(super::plugin_ed_builtins::ED_BUILTINS);
        shell.install_builtins(ral_core::builtins::WATCH_BUILTIN);

        // Install the captured builtins for job-control and plugin
        // lifecycle commands.  This must come after capabilities are applied
        // and before the frontend is created.
        let entries = super::host_handlers::build(jobs.clone(), runtime.clone());
        shell.install_captured_builtins(entries);

        let frontend = boot::create_frontend(
            interactive_mode,
            &mut shell,
            edit_mode,
            bell,
            runtime.clone(),
        );

        Ok(Self {
            shell,
            jobs,
            frontend,
            runtime,
            pending: None,
            exit_code: 0,
        })
    }

    /// Drive the loop until a frontend reports EOF or `exit` escapes the
    /// evaluator.  History flush and job reaping happen in [`Drop`], so
    /// they cover a panic-unwind exit as well as this orderly one.
    pub(super) fn run(self) -> ExitCode {
        ral_core::dbg_trace!("repl", "entering REPL loop");
        let mut session = self;
        while let Flow::Continue = session.turn() {}
        ExitCode::from(session.exit_code)
    }

    /// Run one iteration: reap children, draw prompt, read, eval.
    /// Returns `Break` when the frontend hits EOF or the evaluator
    /// returns an exit code.
    fn turn(&mut self) -> Flow {
        self.jobs.lock().unwrap().reap();

        // A residual interrupt from the prior command must not poison
        // prompt evaluation — by the time we're drawing a new prompt the
        // unwind is done and the flag's only effect would be to make the
        // user's RAL_PROMPT thunk return Break::Error("interrupted").
        ral_core::process::clear();
        write_terminal_title(&self.shell);
        let prompt = render_prompt(&mut self.shell, &self.runtime);

        match self
            .frontend
            .read(&mut self.shell, &prompt, self.pending.take())
        {
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
            &mut self.shell,
            #[cfg(unix)]
            &self.jobs,
            &self.runtime,
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
    /// `Session`, so a crash mid-turn neither orphans a stopped process
    /// group nor drops the session's history.
    fn drop(&mut self) {
        self.frontend.save_history();
        // A panic that poisons the JobTable still leaves it structurally
        // valid for a best-effort SIGTERM/SIGKILL sweep; recover the guard
        // rather than re-panicking into a process abort during unwind.
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cleanup();
    }
}
