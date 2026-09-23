//! One-shot bootstrap for an interactive [`Session`](super::Session): the
//! process-level setup that precedes the engine — signal handlers, terminal
//! claim, panic hook — and the frontend the rc settles on after it.

use ral_core::diagnostic;
use ral_core::io::{InteractiveMode, TerminalState};
use ral_core::protocol::Transport;
use rustyline::config::BellStyle;
use std::sync::Arc;

use super::super::config::RcSettings;
#[cfg(feature = "structural")]
use super::super::frontend::StructuralFrontend;
use super::super::frontend::{Frontend, MinimalFrontend, RustylineFrontend, Surface};
use super::super::host::ReplHost;

/// Install signal handlers and job-control signal masks for interactive use.
/// The handlers raise ambient causes, which [`Session`](super::Session)
/// forwards to its engine as `Control`.
///
/// Unix disposition table:
/// - SIGINT  → interrupt handler (no-op when idle; raises the foreground interrupt)
/// - SIGQUIT → quit handler (Ctrl+\ cancels the durable root — reaping the
///   foreground run and every detached worker — instead of core-dumping)
/// - SIGTERM/SIGHUP → term handler (cancels the durable root with `Terminate`;
///   the third delivery force-exits via the escalation ladder)
/// - SIGTSTP → `SIG_IGN`  (the shell never suspends; a stop is answered with
///   `SIGCONT` by the reaper)
/// - SIGTTOU → `SIG_IGN`  (shell writes terminal settings without being stopped)
/// - SIGTTIN → `SIG_IGN`  (shell reads stdin without being stopped if not fg)
/// - SIGPIPE → `SIG_IGN`  (writing to a closed pipe yields an error, not death)
///
/// SIGWINCH (owned in-process by crossterm's `signal-hook-registry` master
/// handler) and SIGSEGV (claimed by fff-search's crash hook, if installed)
/// must never be named here: a raw install would silently and permanently
/// disconnect that registry's dispatch for the signal.
///
/// Windows: installs `SetConsoleCtrlHandler` via `signal::install_handlers`.
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
        unsafe {
            // The non-escalating interrupt handler rather than SIG_IGN: a
            // no-op between commands, and a cancel of the foreground scope —
            // whence the pipeline's own teardown — during a run.
            libc::signal(
                libc::SIGINT,
                ral_core::process::interrupt_handler() as *const () as libc::sighandler_t,
            );
            // Ctrl-\ cancels the durable root — the reap-everything gesture.
            libc::signal(
                libc::SIGQUIT,
                ral_core::process::quit_handler() as *const () as libc::sighandler_t,
            );
            let term = ral_core::process::term_handler() as *const () as libc::sighandler_t;
            libc::signal(libc::SIGTERM, term);
            libc::signal(libc::SIGHUP, term);
            // Ignore SIGTSTP (the shell never suspends; a stop is answered
            // with SIGCONT by the reaper) and SIGTTOU/SIGTTIN so the shell
            // manipulates the terminal and reads stdin without being stopped
            // when backgrounded.
            libc::signal(libc::SIGTSTP, libc::SIG_IGN);
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
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
    let stdin = rustix::stdio::stdin();
    // Park ourselves out of the parent's way until foregrounded.  Each
    // SIGTTIN stops the whole group; when resumed we re-check, since the
    // foreground may have changed again.
    while rustix::termios::tcgetpgrp(stdin).ok() != Some(rustix::process::getpgrp()) {
        if let Err(e) = rustix::process::kill_current_process_group(rustix::process::Signal::TTIN) {
            return Err(format!("kill(SIGTTIN): {e}"));
        }
    }
    let pid = rustix::process::getpid();
    if rustix::process::getpgrp() != pid
        && let Err(e) = rustix::process::setpgid(None, None)
    {
        return Err(format!("setpgid: {e}"));
    }
    if let Err(e) = rustix::termios::tcsetpgrp(stdin, pid) {
        return Err(format!("tcsetpgrp: {e}"));
    }
    Ok(())
}

/// Save terminal state and install a panic hook that restores it and writes a crash log.
///
/// Unix snapshots termios and restores it with `tcsetattr`; Windows
/// snapshots the console mode and restores it with `SetConsoleMode`
/// (`ral_core::io::console_mode_snapshot`/`restore_console_mode`) — the
/// two platforms' analogues of "undo whatever raw mode left dirty" before
/// [`write_crash_log`] runs.  Either arm is a no-op when stdin isn't a
/// real terminal (no termios / console mode to snapshot).
pub(super) fn setup_panic_hook() {
    #[cfg(unix)]
    {
        let saved = rustix::termios::tcgetattr(rustix::stdio::stdin()).ok();
        if let Some(t) = saved {
            let crash_dir = crash_log_dir();
            std::panic::set_hook(Box::new(move |info| {
                let _ = rustix::termios::tcsetattr(
                    rustix::stdio::stdin(),
                    rustix::termios::OptionalActions::Now,
                    &t,
                );
                write_crash_log(&crash_dir, info);
            }));
        }
    }
    #[cfg(windows)]
    {
        let saved = ral_core::io::console_mode_snapshot();
        if let Some(mode) = saved {
            let crash_dir = crash_log_dir();
            std::panic::set_hook(Box::new(move |info| {
                ral_core::io::restore_console_mode(mode);
                write_crash_log(&crash_dir, info);
            }));
        }
    }
}

/// Crash-log directory (`$XDG_STATE_HOME/ral`), resolved at hook-install
/// time so an unset or changed `HOME` mid-session cannot redirect the
/// crash log.
#[allow(
    clippy::disallowed_methods,
    reason = "host-env: the crash log belongs to the launching user, outside any shell overlay"
)]
fn crash_log_dir() -> std::path::PathBuf {
    let home = ral_core::host::home();
    // A crash report must land somewhere; with no home the temp dir is the
    // honest last resort, where writing it under the cwd would not be.
    ral_core::path::basedir::resolve_xdg(ral_core::path::basedir::XdgKind::State, home.as_deref())
        .unwrap_or_else(std::env::temp_dir)
        .join("ral")
}

/// Write the panic report both platform hooks share: `dir/crash-<unix-ts>.
/// log` holding the panic message and a captured backtrace, after the
/// terminal/console has already been restored by the caller.  Every write
/// (including the stderr notice) ignores errors — a panic hook must not
/// panic.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:crashlog-write] panic hook creates the state dir and writes a crash log; not turn-time model I/O"
)]
fn write_crash_log(dir: &std::path::Path, info: &std::panic::PanicHookInfo<'_>) {
    use std::io::Write as _;
    let _ = std::fs::create_dir_all(dir);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let path = dir.join(format!("crash-{ts}.log")).display().to_string();
    let bt = std::backtrace::Backtrace::force_capture();
    let _ = std::fs::write(&path, format!("{info}\n\n{bt}"));
    let _ = writeln!(std::io::stderr(), "ral: panic — crash log: {path}");
}

/// Build the line-editing frontend from the resolved [`RcSettings`] —
/// whose `surface` the caller has already overridden with the `--surface`
/// flag, if given.
///
/// The capability gate comes first: a terminal resolved to
/// [`InteractiveMode::Minimal`](ral_core::io::InteractiveMode::Minimal) — a
/// dumb terminal or `RAL_INTERACTIVE_MODE=minimal` — can only do the
/// canonical-stdin editor, whatever surface was asked for.  Otherwise the
/// surface preference decides.  A `Structural` request that cannot be
/// honoured — no raw mode, or a binary built without the `structural`
/// feature — warns and falls back to readline rather than degrading
/// silently.
pub(super) fn create_frontend(
    interactive_mode: InteractiveMode,
    settings: &RcSettings,
    engine: Arc<dyn Transport>,
    host: Arc<ReplHost>,
    terminal: TerminalState,
) -> Box<dyn Frontend> {
    if matches!(interactive_mode, InteractiveMode::Minimal) {
        return Box::new(MinimalFrontend::new());
    }
    match settings.surface {
        Surface::Minimal => return Box::new(MinimalFrontend::new()),
        // The structural surface needs raw mode; its `new` probes for it and
        // errors when unavailable, so a failure warns and falls through.
        Surface::Structural => {
            #[cfg(feature = "structural")]
            match StructuralFrontend::new(settings.edit_mode, host.clone()) {
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
    let bell = if settings.bell {
        BellStyle::Audible
    } else {
        BellStyle::None
    };
    Box::new(RustylineFrontend::new(
        engine,
        host,
        terminal,
        settings.edit_mode,
        bell,
    ))
}
