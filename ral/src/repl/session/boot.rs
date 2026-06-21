//! One-shot bootstrap for an interactive [`Session`](super::Session).
//!
//! Each function here is called exactly once at REPL startup and does
//! one process-level setup task: signal handlers, terminal claim, panic
//! hook, terminal capability probe, profile/rc sourcing, frontend
//! construction.  Splitting them out of `session.rs` keeps the state
//! machine itself focused on the run/turn/eval loop.

use ral_core::types::{Break, Escape};
use ral_core::{RequestedTerminalAccess, Shell, TurnReport, diagnostic, evaluator::evaluate};
use rustyline::config::{BellStyle, EditMode};
use std::sync::{Arc, Mutex};

use super::super::config::{RcCtx, create_default_rc, find_ralrc};
#[cfg(feature = "structural")]
use super::super::frontend::StructuralFrontend;
use super::super::frontend::{Frontend, MinimalFrontend, RustylineFrontend, Surface};
use super::super::plugin::{PluginRuntime, framed_turn_request};
#[cfg(unix)]
use crate::jobs;

/// Install signal handlers and job-control signal masks for interactive use.
///
/// Unix disposition summary:
/// - SIGINT  → relay handler (no-op when idle; forwards to external pipeline groups)
/// - SIGTERM/SIGHUP → term handler (sets SIGNAL_COUNT for graceful unwind)
/// - SIGQUIT → quit handler (Ctrl+\ cancels the durable root — reaping the
///   foreground turn and every detached worker — instead of core-dumping;
///   installed by `jobs::setup_signals`, a no-op between turns)
/// - SIGTSTP → SIG_IGN  (shell handles Ctrl+Z via waitpid, not self-stop)
/// - SIGTTOU → SIG_IGN  (shell writes terminal settings without being stopped)
/// - SIGTTIN → SIG_IGN  (shell reads stdin without being stopped if not fg)
/// - SIGPIPE → SIG_IGN  (writing to a closed pipe yields an error, not death)
///
/// Windows: installs SetConsoleCtrlHandler via `signal::install_handlers`.
pub(super) fn setup_signals() {
    #[cfg(unix)]
    {
        // Claim the terminal first, while SIGTTIN still has its default
        // disposition: `claim_terminal` parks the shell on SIGTTIN until it
        // is foregrounded, which the SIG_IGN below would defeat.
        if let Err(msg) = claim_terminal() {
            // A REPL that can't claim its tty is awkward (job control
            // won't work, ^C delivery may misroute) but not fatal — many
            // unusual terminal setups (nested shell-in-pipe, container
            // PID namespaces, mosh sessions) trip these calls.  Warn,
            // keep going.
            diagnostic::shell_warning(&format!(
                "ral: could not claim terminal: {msg}; job control may misbehave"
            ));
        }
        jobs::setup_signals(); // SIGINT relay, SIGQUIT root-abort, SIGTSTP/SIGTTOU ignore
        unsafe {
            let term = ral_core::process::term_handler() as *const () as libc::sighandler_t;
            libc::signal(libc::SIGTERM, term);
            libc::signal(libc::SIGHUP, term);
            libc::signal(libc::SIGTTIN, libc::SIG_IGN);
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        }
    }
    #[cfg(windows)]
    ral_core::process::install_handlers();
}

/// Ensure the shell is the foreground process-group leader of its controlling terminal.
///
/// Wait until the shell is in the foreground, then become the leader of a new
/// process group and claim the terminal via `tcsetpgrp`.  No-op (returns `Ok`)
/// if stdin is not a tty.
///
/// SIGTTOU is ignored by the time we arrive, so a bare `tcsetpgrp` would
/// succeed from a *background* group too — which is the bug: `ral &` launched
/// from an interactive shell would steal the foreground from that shell's
/// current job, and the two would then fight for keystrokes.  Instead we
/// follow the standard job-control init protocol: while another group owns the
/// terminal, stop ourselves with SIGTTIN and only proceed once the user (or the
/// parent shell) has foregrounded us.  SIGTTIN must be at its default
/// disposition for the stop to take effect; `setup_signals` ignores it only
/// after this returns.
///
/// `setpgid` is skipped when pgid already equals pid — that covers both the
/// trivial no-op case and a session leader, on which `setpgid` returns EPERM.
///
/// Failure of either `setpgid` or `tcsetpgrp` is reported as the underlying
/// `errno` message; callers decide whether to abort or carry on degraded.
#[cfg(unix)]
fn claim_terminal() -> Result<(), String> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return Ok(());
    }
    unsafe {
        // Park ourselves out of the parent's way until foregrounded.  Each
        // SIGTTIN stops the whole group; when resumed we re-check, since the
        // foreground may have changed again.
        while libc::tcgetpgrp(libc::STDIN_FILENO) != libc::getpgrp() {
            if libc::kill(0, libc::SIGTTIN) == -1 {
                return Err(format!(
                    "kill(SIGTTIN): {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        let pid = libc::getpid();
        if libc::getpgrp() != pid && libc::setpgid(0, 0) == -1 {
            return Err(format!("setpgid: {}", std::io::Error::last_os_error()));
        }
        if libc::tcsetpgrp(libc::STDIN_FILENO, pid) == -1 {
            return Err(format!("tcsetpgrp: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// Save terminal state and install a panic hook that restores it and writes a crash log.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:crashlog-write] panic hook creates the state dir and writes a crash log; not turn-time model I/O"
)]
pub(super) fn setup_panic_hook() {
    #[cfg(unix)]
    {
        let saved = ral_core::process::termios_snapshot();
        if let Some(t) = saved {
            let home = crate::platform::home_dir();
            std::panic::set_hook(Box::new(move |info| {
                unsafe {
                    libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t);
                }
                let dir = ral_core::path::basedir::resolve_xdg(
                    ral_core::path::basedir::XdgKind::State,
                    &home,
                )
                .join("ral");
                let _ = std::fs::create_dir_all(&dir);
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let path = dir.join(format!("crash-{ts}.log")).display().to_string();
                let bt = std::backtrace::Backtrace::force_capture();
                let _ = std::fs::write(&path, format!("{info}\n\n{bt}"));
                eprintln!("ral: panic — crash log: {path}");
            }));
        }
    }
}

/// Bind the default `RAL_PROMPT` thunk, returning
/// [`DEFAULT_PROMPT`](super::super::prompt::DEFAULT_PROMPT), in the
/// session scope.
///
/// Runs after builtin/prelude registration and before rc sourcing, so the
/// rc `prompt:` key (or a plain `let RAL_PROMPT = …`) overwrites it.  Value
/// bindings can be overwritten but never removed, so every booted session
/// has `RAL_PROMPT` bound — [`render`](super::super::prompt::render)
/// relies on this.
pub(super) fn install_default_prompt(shell: &mut Shell) {
    let src = format!("{{ return \"{}\" }}", super::super::prompt::DEFAULT_PROMPT);
    let comp = Arc::new(ral_core::compile(&src).expect("default prompt thunk compiles"));
    let block = evaluate(&comp, shell).expect("default prompt thunk evaluates");
    shell.mobile.scope.set("RAL_PROMPT".into(), block);
}

/// Mark the shell interactive and publish the probed terminal as the
/// `TERMINAL` scope binding.  The terminal itself was probed before the
/// shell was constructed and is exposed via `shell.terminal()`.
pub(super) fn setup_terminal(shell: &mut Shell) {
    shell.set_interactive(true);
    shell
        .mobile
        .scope
        .set("TERMINAL".into(), shell.terminal().to_value());
}

/// Source login profiles (if login shell) and the user RC file.
///
/// Login profiles: `/etc/ral/profile`, then `~/.ral_profile`.
/// RC: `$XDG_CONFIG_HOME/ral/rc` or `~/.ralrc` (created from a default
/// skeleton if neither exists).  Each file is parsed as ral source and
/// its return value is fed to [`apply_rc_config`](super::super::config::apply_rc_config).
pub(super) fn load_profiles(is_login: bool, no_rc: bool, ctx: &mut RcCtx<'_>) {
    if is_login {
        let system_profile = "/etc/ral/profile".to_string();
        let user_profile = ral_core::path::config::home_dot(".ral_profile")
            .map(|p| p.to_string_lossy().into_owned());
        for path in [Some(system_profile), user_profile].into_iter().flatten() {
            if ral_core::path::exists(&path) {
                source_config_file(&path, ctx);
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
            source_config_file(&rc_path, ctx);
        }
    }
}

/// Build the line-editing frontend from the requested [`Surface`].
///
/// The capability gate comes first: a terminal resolved to
/// [`InteractiveMode::Minimal`](ral_core::io::InteractiveMode::Minimal) — a
/// dumb terminal or `RAL_INTERACTIVE_MODE=minimal` — can only do the
/// canonical-stdin editor, whatever surface was asked for.  Otherwise the
/// surface preference (`--surface` flag or rc `surface:`) decides.  A
/// `Structural` request that cannot be honoured — no raw mode, or a binary
/// built without the `structural` feature — warns and falls back to readline
/// rather than degrading silently.
///
/// For the readline path, also wires up an `ExternalPrinter` sink on
/// stdout via `shell.set_stdout(…)` so background output from `watch` blocks appears above
/// the active prompt.
pub(super) fn create_frontend(
    interactive_mode: ral_core::io::InteractiveMode,
    surface: Surface,
    shell: &mut Shell,
    edit_mode: EditMode,
    bell: BellStyle,
    runtime: Arc<Mutex<PluginRuntime>>,
) -> Box<dyn Frontend> {
    if matches!(interactive_mode, ral_core::io::InteractiveMode::Minimal) {
        return Box::new(MinimalFrontend::new());
    }
    match surface {
        Surface::Minimal => return Box::new(MinimalFrontend::new()),
        // The structural surface needs raw mode; its `new` probes for it and
        // errors when unavailable, so a failure warns and falls through.
        Surface::Structural => {
            #[cfg(feature = "structural")]
            match StructuralFrontend::new(edit_mode, runtime.clone()) {
                Ok(fe) => return Box::new(fe),
                Err(_) => diagnostic::shell_warning(
                    "ral: structural surface needs a raw-mode terminal; using readline",
                ),
            }
            #[cfg(not(feature = "structural"))]
            diagnostic::shell_warning("ral: this build has no structural surface; using readline");
        }
        Surface::Readline => {}
    }
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
        shell.set_stdout(ral_core::io::Sink::External(Arc::new(RustylineSink(
            StdMutex::new(printer),
        ))));
    }

    Box::new(rl_fe)
}

// ── RC sourcing ─────────────────────────────────────────────────────────

/// Parse and evaluate a config file, applying the resulting map via
/// [`apply_rc_config`](super::super::config::apply_rc_config) and running
/// its `startup` block if present.  The fallible body lives in
/// [`source_config_inner`]; this wrapper just surfaces the one diagnostic
/// message its `?` chain produces.
fn source_config_file(path: &str, ctx: &mut RcCtx<'_>) {
    if let Err(msg) = source_config_inner(path, ctx) {
        diagnostic::cmd_error("ral", &msg);
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:config-read] reads an rc/config file during session boot; not turn-time model I/O"
)]
fn source_config_inner(path: &str, ctx: &mut RcCtx<'_>) -> Result<(), String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let src = ral_core::source::normalize_source_text(src);
    let comp = ral_core::compile(&src).map_err(|e| format!("{path}: {e}"))?;
    // The rc check always runs (it writes the evaluator's mode wires) and is
    // seeded from the live shell, so an earlier rc file's bindings are
    // visible to a later file's check.  A type error of any kind leaves the
    // file with no runnable annotation: it is reported and skipped while the
    // boot survives — a broken rc must not strand the user at no shell, the
    // parse-error precedent above.
    let comp = match ral_core::typecheck(&comp, ctx.shell.session_schemes()) {
        Ok(annotated) => std::sync::Arc::new(annotated),
        Err(errs) => {
            eprint!(
                "{}",
                diagnostic::format_type_errors_ariadne(path, &src, &errs)
            );
            return Err(format!("{path}: skipped due to type errors"));
        }
    };
    // Top-level eval: `evaluate` absorbs tail calls and returns
    // `Settled<Value>`.  Match directly on `Break`.
    let config = match evaluate(&comp, ctx.shell) {
        Ok(v) => v,
        Err(Break::Error(e)) => return Err(format!("{path}: {}", e.message)),
        // `exit` in rc: stop sourcing, boot continues.
        Err(Break::Escape(Escape::Exit(_))) => return Ok(()),
        // A stop signal (Ctrl-Z during a slow rc command) parks a process
        // group the REPL job table never learned about — never resumed,
        // never reaped.  Report it like the `startup:` arm below rather
        // than silently orphaning the group.
        #[cfg(unix)]
        Err(Break::Escape(Escape::Stopped { .. })) => {
            return Err(format!(
                "{path}: a stop signal escaped — rc files cannot park jobs"
            ));
        }
    };
    if let Some(block) = super::super::config::apply_rc_config(config, ctx, Some(&src)) {
        // The startup block runs through the value turn door: a fresh frame
        // with the session's live streams and `Denied` terminal authority (an
        // rc block must not foreground a child). `Block` discards its mobile on
        // exit, so `let`s inside it do not leak — the prior in-place `apply`
        // behaviour, now properly framed.
        let req = framed_turn_request("<startup>", RequestedTerminalAccess::Denied);
        match ctx.shell.run_value_turn(block, vec![], "", req) {
            TurnReport::Ran { result, .. } => match result {
                Ok(_) | Err(Break::Escape(Escape::Exit(_))) => {}
                Err(Break::Error(e)) => return Err(format!("{path}: startup: {}", e.message)),
                #[cfg(unix)]
                Err(Break::Escape(Escape::Stopped { .. })) => {
                    return Err(format!(
                        "{path}: startup: a stop signal escaped — rc files cannot park jobs"
                    ));
                }
            },
            TurnReport::Static { .. } => {
                unreachable!("a thunk startup block never compiles source")
            }
        }
    }
    Ok(())
}
