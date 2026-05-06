//! Interactive read-eval-print loop for the `ral` shell.
//!
//! Orchestrates terminal setup, signal handling, profile loading, and the
//! main readline loop.  The actual line editor lives behind the [`Frontend`]
//! trait, with two implementations: [`RustylineFrontend`] (full line editing,
//! completion, plugin hooks) and [`MinimalFrontend`] (raw stdin, no termios
//! manipulation, for dumb terminals and `RAL_INTERACTIVE_MODE=minimal`).
//!
//! Submodules factor out orthogonal concerns:
//! - [`builtins`] -- REPL-only commands (`cd`, `jobs`, `fg`, `bg`, `disown`).
//! - [`complete`] -- Tab completion (commands, variables, paths).
//! - [`config`]   -- RC file discovery, parsing, and application.
//! - [`cursor`]   -- ANSI cursor-position queries (Unix only).
//! - [`errfmt`]   -- REPL-specific error formatting helpers.
//! - [`exec`]     -- Single-input parse/typecheck/eval cycle.
//! - [`frontend`] -- The `Frontend` trait and its two implementations.
//! - [`keybinding`] -- Plugin keybinding dispatch.
//! - [`plugin`]   -- Plugin runtime state and hook machinery.
//! - [`prompt`]   -- Prompt construction and thunk evaluation.

mod complete;
mod config;
mod plugin;

mod builtins;
#[cfg(unix)]
mod cursor;
mod errfmt;
mod exec;
mod frontend;
mod keybinding;
mod prompt;

use config::{RcCtx, create_default_rc, find_ralrc, terminal_capability_map};
use exec::{Step, step};
use frontend::{Frontend, FrontendEnd, KeybindingResolution, MinimalFrontend, RustylineFrontend};
use plugin::PluginRuntime;
use prompt::build_prompt;

use ral_core::{Shell, EvalSignal, eval_comp};
use ral_core::{builtins as ral_builtins, diagnostic};
use rustyline::config::{BellStyle, EditMode};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use super::jobs;

/// Enter the interactive REPL.
///
/// Sets up signals, terminal state, builtins, profiles/RC, and the line
/// editor, then loops: prompt, readline, resolve keybindings, dispatch.
/// Returns the exit code for the process.
pub(crate) fn run_interactive(is_login: bool, opts: &super::InteractiveOpts) -> ExitCode {
    setup_signals();
    let mut shell = Shell::new(Default::default()); // setup_terminal() below probes real state
    super::seed_default_env(&mut shell);
    setup_panic_hook();

    // Login shell: set umask and source system/user profiles.
    #[cfg(unix)]
    if is_login {
        unsafe {
            libc::umask(0o022);
        }
    }

    let interactive_mode = setup_terminal(&mut shell);
    let mut edit_mode = EditMode::Emacs;
    let mut bell = BellStyle::None;
    ral_core::dbg_trace!("repl", "before register");
    ral_builtins::register(&mut shell, super::baked_prelude_comp());
    ral_core::dbg_trace!("repl", "after register");

    #[cfg(unix)]
    let mut job_table = jobs::JobTable::new();

    load_profiles(
        is_login,
        opts.no_rc,
        opts.run.no_typecheck,
        &mut RcCtx {
            shell: &mut shell,
            edit_mode: &mut edit_mode,
            bell: &mut bell,
        },
    );
    // CLI flag wins over rc — apply after load_profiles.
    if let Some(n) = opts.run.recursion_limit {
        shell.control.recursion_limit = n;
    }
    let mut fe = create_frontend(interactive_mode, &mut shell, edit_mode, bell);

    ral_core::dbg_trace!("repl", "entering REPL loop");

    let mut pending: Option<(String, usize)> = None;
    let mut exit_code: u8 = 0;

    loop {
        #[cfg(unix)]
        job_table.reap();

        if pending.is_none() {
            pending = fe.drain_buffer_stack();
        }

        fe.before_readline(&shell);
        let prompt = build_prompt(&mut shell);

        let raw = match fe.readline(&prompt, pending.take(), &shell) {
            Ok(s) => s,
            Err(FrontendEnd::Interrupted) => {
                ral_core::signal::clear();
                continue;
            }
            Err(FrontendEnd::Eof) => break,
        };

        match fe.resolve_keybinding(raw, &mut shell) {
            KeybindingResolution::Requeue(text, cursor) => {
                // Erase the stray newline rustyline emits on AcceptLine,
                // *then* flush any plugin diagnostics so they land on a
                // durable line above the next prompt.  Order matters:
                // printing before the escape would have its line clobbered.
                if shell.io.terminal.startup_stdout_tty {
                    use std::io::Write;
                    let _ = std::io::stdout().write_all(b"\x1b[A\r\x1b[K");
                    let _ = std::io::stdout().flush();
                }
                fe.flush_plugin_messages();
                pending = Some((text, cursor));
                continue;
            }
            KeybindingResolution::Proceed(input) => {
                // No line-erase escape on the Proceed path, but the
                // keybinding handler (or buffer-change hook) may still have
                // produced diagnostics worth surfacing before we run the
                // resulting input.
                fe.flush_plugin_messages();
                let trimmed = input.trim();
                if trimmed.is_empty() {
                    continue;
                }
                fe.add_history(trimmed);

                #[cfg(unix)]
                let stdin_tty = shell.io.terminal.startup_stdin_tty;
                match step(
                    trimmed,
                    &mut shell,
                    #[cfg(unix)]
                    &mut job_table,
                    #[cfg(unix)]
                    stdin_tty,
                ) {
                    Step::Continue => {}
                    Step::Exit(c) => {
                        exit_code = c;
                        break;
                    }
                }
            }
        }
    }

    fe.save_history();
    #[cfg(unix)]
    job_table.cleanup();

    ExitCode::from(exit_code)
}

// ── Setup phases ─────────────────────────────────────────────────────────

/// Install signal handlers and job-control signal masks for interactive use.
///
/// Unix disposition summary:
/// - SIGINT  → relay handler (no-op when idle; forwards to external pipeline groups)
/// - SIGTERM/SIGHUP → term handler (sets SIGNAL_COUNT for graceful unwind)
/// - SIGQUIT → SIG_IGN  (Ctrl+\ must not kill or core-dump the shell)
/// - SIGTSTP → SIG_IGN  (shell handles Ctrl+Z via waitpid, not self-stop)
/// - SIGTTOU → SIG_IGN  (shell writes terminal settings without being stopped)
/// - SIGTTIN → SIG_IGN  (shell reads stdin without being stopped if not fg)
/// - SIGPIPE → SIG_IGN  (writing to a closed pipe yields an error, not death)
///
/// Windows: installs SetConsoleCtrlHandler via `signal::install_handlers`.
fn setup_signals() {
    #[cfg(unix)]
    {
        jobs::setup_signals(); // SIGINT relay, SIGTSTP/SIGTTOU ignore
        unsafe {
            let term = ral_core::signal::term_handler() as *const () as libc::sighandler_t;
            libc::signal(libc::SIGTERM, term);
            libc::signal(libc::SIGHUP, term);
            libc::signal(libc::SIGQUIT, libc::SIG_IGN);
            libc::signal(libc::SIGTTIN, libc::SIG_IGN);
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        }
        claim_terminal();
    }
    #[cfg(windows)]
    ral_core::signal::install_handlers();
}

/// Ensure the shell is the foreground process-group leader of its controlling terminal.
///
/// Becomes the leader of a new process group, then claims the terminal via
/// `tcsetpgrp`.  SIGTTOU is already ignored by the time we arrive here, so
/// `tcsetpgrp` succeeds even when invoked from a background group — no spin
/// loop required.  No-op if stdin is not a tty.
#[cfg(unix)]
fn claim_terminal() {
    {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            return;
        }
    }
    unsafe {
        let pid = libc::getpid();
        libc::setpgid(0, 0);
        libc::tcsetpgrp(libc::STDIN_FILENO, pid);
    }
}

/// Save terminal state and install a panic hook that restores it and writes a crash log.
fn setup_panic_hook() {
    #[cfg(unix)]
    {
        let saved: Option<libc::termios> = unsafe {
            let mut t = std::mem::zeroed::<libc::termios>();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut t) == 0 {
                Some(t)
            } else {
                None
            }
        };
        if let Some(t) = saved {
            let home = super::home_dir();
            std::panic::set_hook(Box::new(move |info| {
                unsafe {
                    libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t);
                }
                let dir = std::env::var("XDG_STATE_HOME")
                    .unwrap_or_else(|_| format!("{home}/.local/state"));
                let dir = format!("{dir}/ral");
                let _ = std::fs::create_dir_all(&dir);
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let path = format!("{dir}/crash-{ts}.log");
                let bt = std::backtrace::Backtrace::force_capture();
                let _ = std::fs::write(&path, format!("{info}\n\n{bt}"));
                eprintln!("ral: panic — crash log: {path}");
            }));
        }
    }
}

/// Probe terminal capabilities, wire up diagnostic color, and set the TERMINAL binding.
/// Returns the resolved `InteractiveMode` (needed for frontend selection).
fn setup_terminal(shell: &mut Shell) -> ral_core::io::InteractiveMode {
    let (mode, terminal) = super::probe_terminal(true);
    shell.io.terminal = terminal;
    shell.io.interactive = true;
    shell.set("TERMINAL".into(), terminal_capability_map(&shell.io.terminal));
    mode
}

/// Source login profiles (if login shell) and the user RC file.
///
/// Login profiles: `/etc/ral/profile`, then `~/.ral_profile`.
/// RC: `$XDG_CONFIG_HOME/ral/rc` or `~/.ralrc` (created from a default
/// skeleton if neither exists).  Each file is parsed as ral source and
/// its return value is fed to [`apply_rc_config`].
fn load_profiles(is_login: bool, no_rc: bool, no_typecheck: bool, ctx: &mut RcCtx<'_>) {
    if is_login {
        for path in [
            "/etc/ral/profile".to_string(),
            format!("{}/.ral_profile", super::home_dir()),
        ] {
            if std::path::Path::new(&path).exists() {
                source_config_file(&path, no_typecheck, ctx);
            }
        }
    }
    if !no_rc {
        ral_core::dbg_trace!("repl", "looking for ralrc: {:?}", find_ralrc());
        let rc_path = find_ralrc().or_else(|| {
            let path = create_default_rc()?;
            eprintln!("note: created {path}");
            Some(path)
        });
        if let Some(rc_path) = rc_path {
            source_config_file(&rc_path, no_typecheck, ctx);
        }
    }
}

/// Build the line-editing frontend: minimal (dumb) or rustyline.
///
/// For the rustyline path, also wires up an `ExternalPrinter` sink on
/// `shell.io.stdout` so background output from `watch` blocks appears above
/// the active prompt.
fn create_frontend(
    interactive_mode: ral_core::io::InteractiveMode,
    shell: &mut Shell,
    edit_mode: EditMode,
    bell: BellStyle,
) -> Box<dyn Frontend> {
    if matches!(interactive_mode, ral_core::io::InteractiveMode::Minimal) {
        return Box::new(MinimalFrontend::new());
    }
    let runtime = Arc::new(Mutex::new(PluginRuntime::default()));
    let mut rl_fe = RustylineFrontend::new(shell, edit_mode, bell, runtime);

    if let Ok(printer) = rl_fe.rl.create_external_printer() {
        use std::sync::Mutex as StdMutex;
        struct RustylineSink<P: rustyline::ExternalPrinter + Send>(StdMutex<P>);
        impl<P: rustyline::ExternalPrinter + Send + 'static> ral_core::io::ExternalWrite
            for RustylineSink<P>
        {
            fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
                let s = String::from_utf8_lossy(bytes).into_owned();
                if let Ok(mut p) = self.0.lock() {
                    p.print(s)
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                }
                Ok(())
            }
        }
        shell.io.stdout =
            ral_core::io::Sink::External(Arc::new(RustylineSink(StdMutex::new(printer))));
    }

    Box::new(rl_fe)
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Parse and evaluate a config file, applying the resulting map via
/// [`apply_rc_config`] and running its `startup` block if present.  The
/// fallible body lives in [`source_config_inner`]; this wrapper just
/// surfaces the one diagnostic message its `?` chain produces.
fn source_config_file(path: &str, no_typecheck: bool, ctx: &mut RcCtx<'_>) {
    if let Err(msg) = source_config_inner(path, no_typecheck, ctx) {
        diagnostic::cmd_error("ral", &msg);
    }
}

fn source_config_inner(path: &str, no_typecheck: bool, ctx: &mut RcCtx<'_>) -> Result<(), String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let comp = ral_core::compile(&src).map_err(|e| format!("{path}: {e}"))?;
    if !no_typecheck {
        let type_errors = ral_core::typecheck(&comp, super::baked_prelude_schemes());
        if !type_errors.is_empty() {
            for e in &type_errors {
                eprint!("{}", diagnostic::format_type_error_ariadne(path, &src, e));
            }
            return Err(format!("{path}: skipped due to type errors"));
        }
    }
    let config = match eval_comp(&comp, ctx.shell) {
        Ok(v) => v,
        Err(EvalSignal::Error(e)) => return Err(format!("{path}: {}", e.message)),
        Err(_) => return Ok(()),
    };
    if let Some(block) = config::apply_rc_config(config, ctx, Some(&src)) {
        match ral_core::call_value_pub(&block, &[], ctx.shell) {
            Ok(_) | Err(EvalSignal::Exit(_)) | Err(EvalSignal::TailCall { .. }) => {}
            Err(EvalSignal::Error(e)) => return Err(format!("{path}: startup: {}", e.message)),
        }
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::complete::{complete_path, should_offer_path_candidate};
    use super::config::apply_rc_config;
    use super::errfmt::format_repl_parse_error;
    use super::prompt::{PromptBindings, eval_prompt_block};
    use super::*;
    use ral_core::Value;

    /// Apply `config` to a fresh shell via `apply_rc_config` and return the shell.
    fn apply_to_fresh_env(config: Value) -> Shell {
        let mut shell = Shell::new(Default::default());
        let mut mode = EditMode::Emacs;
        let mut bell = BellStyle::None;
        apply_rc_config(
            config,
            &mut RcCtx { shell: &mut shell, edit_mode: &mut mode, bell: &mut bell },
            None,
        );
        shell
    }

    #[test]
    fn rc_bindings_populate_value_namespace() {
        let shell = apply_to_fresh_env(Value::Map(vec![(
            "bindings".into(),
            Value::Map(vec![
                ("greeting".into(), Value::String("hello".into())),
                ("n".into(), Value::Int(42)),
            ]),
        )]));
        assert_eq!(shell.get("greeting"), Some(&Value::String("hello".into())));
        assert_eq!(shell.get("n"), Some(&Value::Int(42)));
    }

    #[test]
    fn rc_aliases_stay_in_command_namespace() {
        let alias = Value::Bool(true);
        let shell = apply_to_fresh_env(Value::Map(vec![(
            "aliases".into(),
            Value::Map(vec![("ll".into(), alias.clone())]),
        )]));
        assert_eq!(shell.registry.aliases.get("ll").map(|e| &e.value), Some(&alias));
        assert!(shell.get("ll").is_none());
    }

    #[test]
    fn repl_compact_parse_error_for_single_value() {
        let rendered = format_repl_parse_error(
            "value cannot appear in command position; use 'return <value>'",
        );
        assert!(rendered.contains("value cannot appear in command position"));
        assert!(rendered.contains("(exit status 2)"));
        assert!(!rendered.contains("-->"));
    }

    /// Parse and evaluate `src` to a thunk against a prelude-loaded shell.
    /// Returns `(shell, prompt_thunk)`.
    fn evaluate_prompt_src(src: &str) -> (Shell, Value) {
        let mut shell = Shell::new(Default::default());
        ral_core::builtins::register(&mut shell, super::super::baked_prelude_comp());
        let ast = ral_core::parse(src).unwrap();
        let comp = ral_core::elaborator::elaborate(&ast, Default::default());
        let prompt = ral_core::evaluate(&comp, &mut shell).unwrap();
        assert!(matches!(prompt, Value::Thunk { .. }), "expected thunk");
        (shell, prompt)
    }

    #[test]
    fn prompt_block_prefers_return_value_over_stdout() {
        let (shell, prompt) = evaluate_prompt_src("{ echo Darwin; return 'ral $ ' }");
        let bindings = PromptBindings::with("u", "/", 0);
        assert_eq!(eval_prompt_block(&prompt, &shell, &bindings), "ral $ ");
    }

    #[test]
    fn prompt_block_keeps_closure_captures_from_rc_scope() {
        let (shell, prompt) = evaluate_prompt_src(
            "let left = '['\n let right = ']'\n return { return \"$left ok $right\" }",
        );
        let bindings = PromptBindings::with("u", "/", 0);
        assert_eq!(eval_prompt_block(&prompt, &shell, &bindings), "[ ok ]");
    }

    #[test]
    fn prompt_block_sees_dynamic_prompt_bindings() {
        let (shell, prompt) = evaluate_prompt_src("return { return \"$USER:$CWD:$STATUS\" }");
        let bindings = PromptBindings::with("alice", "~/src", 7);
        assert_eq!(eval_prompt_block(&prompt, &shell, &bindings), "alice:~/src:7");
    }

    #[test]
    fn complete_path_expands_home_tilde_prefix() {
        if crate::platform::home_dir() == "." {
            return;
        }
        let (start, _) = complete_path("~/", 0).unwrap();
        assert_eq!(start, 2);
    }

    #[test]
    fn complete_path_supports_bare_tilde_token() {
        if crate::platform::home_dir() == "." {
            return;
        }
        let (start, _) = complete_path("~", 3).unwrap();
        assert_eq!(start, 3);
    }

    #[test]
    fn path_completion_hides_dotfiles_unless_prefix_has_dot() {
        assert!(!should_offer_path_candidate(".git", ""));
        assert!(!should_offer_path_candidate(".git", "g"));
        assert!(should_offer_path_candidate(".git", "."));
        assert!(should_offer_path_candidate("src", ""));
    }
}
