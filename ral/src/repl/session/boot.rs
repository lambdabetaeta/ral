//! One-shot bootstrap for an interactive [`Session`](super::Session): the
//! process-level setup that precedes the engine — signal handlers, terminal
//! claim, panic hook — and the frontend the rc settles on after it.

use ral_core::carrier::Transport;
use ral_core::own::OwnDir;
use ral_core::path::{Visibility, XdgKind};
use ral_core::terminal::{self, InteractiveMode, TerminalState};
use rustyline::config::BellStyle;
use std::sync::Arc;

use super::super::config::RcSettings;
use super::super::frontend::{Frontend, MinimalFrontend, RustylineFrontend, Surface};
use super::super::host::ReplHost;

/// Install signal handlers and job-control signal masks for interactive use,
/// and claim the terminal for the session to hold until it ends.  The
/// handlers raise ambient causes, which [`Session`](super::Session) forwards
/// to its engine as `Control`.
///
/// Unix disposition table:
/// - SIGINT  → interrupt handler (no-op when idle; raises the foreground interrupt)
/// - SIGQUIT → quit handler (Ctrl+\ cancels the durable root — reaping the
///   foreground run and every worker — instead of core-dumping)
/// - SIGTERM/SIGHUP → term handler (cancels the durable root with `Terminate`;
///   the third delivery force-exits via the escalation ladder)
/// - SIGTSTP → `SIG_IGN`  (the shell never suspends; a stop is answered with
///   `SIGCONT` by the reaper)
/// - SIGTTOU → `SIG_IGN`  (shell writes terminal settings without being stopped)
/// - SIGTTIN → `SIG_IGN`  (shell reads stdin without being stopped if not fg)
/// - SIGPIPE → `SIG_IGN`  (as Rust leaves it: a write to a closed pipe is an error, and on
///   the host's stdout the run's stop)
///
/// Every disposition goes through core's signal installers, which snapshot
/// the inherited ignores first so children get them back (the nohup rule).
///
/// SIGWINCH (owned in-process by crossterm's `signal-hook-registry` master
/// handler) must never be named here: a raw install would silently and
/// permanently disconnect that registry's dispatch for it.
///
/// Windows: `signal::install_interrupt_handler`, likewise non-escalating.
#[cfg_attr(windows, allow(clippy::unnecessary_wraps))]
pub(super) fn setup_signals() -> std::io::Result<TerminalClaim> {
    #[cfg(unix)]
    {
        use ral_core::process::{default, ignore, install};
        // Ignore SIGTTOU before the claim, whose `tcsetpgrp` runs from a
        // group not yet in the foreground; SIGTTIN must stop the shell
        // until after, since the claim parks it there.
        ignore(libc::SIGTTOU);
        default(libc::SIGTTIN);
        let claim = claim_terminal().unwrap_or_else(|msg| {
            // A REPL that can't claim its tty is awkward (job control
            // won't work, ^C delivery may misroute) but not fatal — many
            // unusual terminal setups (nested shell-in-pipe, container
            // PID namespaces, mosh sessions) trip these calls.  Warn,
            // keep going.
            terminal::shell_warning(&format!(
                "ral: could not claim terminal: {msg}; job control may misbehave"
            ));
            TerminalClaim::default()
        });
        // The non-escalating interrupt handler rather than SIG_IGN: a
        // no-op between commands, and a cancel of the foreground scope —
        // whence the pipeline's own teardown — during a run.
        ral_core::process::install_interrupt_handler()?;
        // Ctrl-\ cancels the durable root — the reap-everything gesture.
        install(libc::SIGQUIT, ral_core::process::quit_handler());
        install(libc::SIGTERM, ral_core::process::term_handler());
        install(libc::SIGHUP, ral_core::process::term_handler());
        // The shell never suspends, and reads stdin without being stopped
        // when backgrounded.
        ignore(libc::SIGTSTP);
        ignore(libc::SIGTTIN);
        ignore(libc::SIGPIPE);
        Ok(claim)
    }
    #[cfg(windows)]
    {
        ral_core::process::install_interrupt_handler();
        Ok(TerminalClaim::default())
    }
}

/// The terminal's foreground, taken from the process group the shell started
/// in and handed back to it on drop.  Without the handback a parent that does
/// no job control — `vim`'s `:sh`, `su` — is left in the background, and
/// stops on its next terminal read.
#[derive(Default)]
pub(super) struct TerminalClaim {
    /// The group left behind; `None` when the shell already led its own.
    #[cfg(unix)]
    left: Option<rustix::process::Pid>,
}

#[cfg(unix)]
impl Drop for TerminalClaim {
    fn drop(&mut self) {
        if let Some(group) = self.left {
            let _ = rustix::termios::tcsetpgrp(rustix::stdio::stdin(), group);
        }
    }
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
/// Otherwise the group left is recorded, for the claim to hand back.
///
/// Failure of either `setpgid` or `tcsetpgrp` is reported as the underlying
/// `errno` message; callers decide whether to abort or carry on degraded.
#[cfg(unix)]
fn claim_terminal() -> Result<TerminalClaim, String> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return Ok(TerminalClaim::default());
    }
    let stdin = rustix::stdio::stdin();
    // Park ourselves out of the parent's way until foregrounded.  Each
    // SIGTTIN stops the whole group; when resumed we re-check, since the
    // foreground may have changed again.  A bounded number of parks: a
    // terminal whose foreground group is gone would stop us for ever.
    let group = rustix::process::getpgrp();
    let mut parks = 0..64;
    while rustix::termios::tcgetpgrp(stdin).map_err(|e| format!("tcgetpgrp: {e}"))? != group {
        parks.next().ok_or(
            "the terminal's foreground belongs to another process group; \
             was ral started in the background of a shell that has since exited?",
        )?;
        rustix::process::kill_current_process_group(rustix::process::Signal::TTIN)
            .map_err(|e| format!("kill(SIGTTIN): {e}"))?;
    }
    let pid = rustix::process::getpid();
    let left = if group == pid {
        None
    } else {
        rustix::process::setpgid(None, None).map_err(|e| format!("setpgid: {e}"))?;
        Some(group)
    };
    rustix::termios::tcsetpgrp(stdin, pid).map_err(|e| format!("tcsetpgrp: {e}"))?;
    Ok(TerminalClaim { left })
}

/// Save terminal state and install a panic hook that restores it and writes a crash log.
///
/// Unix snapshots termios and restores it with `tcsetattr`; Windows
/// snapshots the console mode and restores it with `SetConsoleMode`
/// (`ral_core::terminal::console_mode_snapshot`/`restore_console_mode`) — the
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
                write_crash_log(crash_dir.as_ref(), info);
            }));
        }
    }
    #[cfg(windows)]
    {
        let saved = ral_core::terminal::console_mode_snapshot();
        if let Some(mode) = saved {
            let crash_dir = crash_log_dir();
            std::panic::set_hook(Box::new(move |info| {
                ral_core::terminal::restore_console_mode(mode);
                write_crash_log(crash_dir.as_ref(), info);
            }));
        }
    }
}

/// Crash-log directory (`$XDG_STATE_HOME/ral`), held from hook-install time
/// so an unset or changed `HOME` mid-session cannot redirect the crash log.
/// `None` where the host has no state base: a crash then leaves no log.
fn crash_log_dir() -> Option<OwnDir> {
    OwnDir::xdg("ral", XdgKind::State).ok()
}

/// Write the panic report both platform hooks share: a new file in `dir`,
/// `crash-<unix-ts>-<random>.log`, holding the panic message and a captured
/// backtrace, after the terminal/console has already been restored by the
/// caller.  Where no log could be kept, the panic message goes to stderr in its
/// place.  Every write (including the stderr notice) ignores errors — a panic
/// hook must not panic.
fn write_crash_log(dir: Option<&OwnDir>, info: &std::panic::PanicHookInfo<'_>) {
    use std::io::Write as _;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let bt = std::backtrace::Backtrace::force_capture();
    let kept = dir.and_then(|dir| {
        let (name, mut file) = dir
            .create_fresh(&format!("crash-{ts}-"), ".log", Visibility::Private)
            .ok()?;
        let _ = file.write_all(format!("{info}\n\n{bt}").as_bytes());
        Some(dir.path().join(name))
    });
    let _ = match kept {
        Some(path) => writeln!(
            std::io::stderr(),
            "ral: panic; crash log: {}",
            ral_core::path::shown(&path)
        ),
        None => writeln!(std::io::stderr(), "ral: panic: {info}"),
    };
}

/// Build the line-editing frontend from the resolved [`RcSettings`] —
/// whose `surface` the caller has already overridden with the `--surface`
/// flag, if given.
///
/// The capability gate comes first: a terminal resolved to
/// [`InteractiveMode::Minimal`](ral_core::terminal::InteractiveMode::Minimal) — a
/// dumb terminal or `RAL_INTERACTIVE_MODE=minimal` — can only do the
/// canonical-stdin editor, whatever surface was asked for.  Otherwise the
/// surface preference decides.
pub(super) fn create_frontend(
    interactive_mode: InteractiveMode,
    settings: &RcSettings,
    engine: Arc<dyn Transport>,
    host: Arc<ReplHost>,
    terminal: TerminalState,
    history: Option<std::path::PathBuf>,
) -> Box<dyn Frontend> {
    if !matches!(interactive_mode, InteractiveMode::Minimal)
        && matches!(settings.surface, Surface::Readline)
    {
        let bell = if settings.bell {
            BellStyle::Audible
        } else {
            BellStyle::None
        };
        let editor = RustylineFrontend::new(
            engine,
            host,
            terminal,
            settings.edit_mode,
            bell,
            history.clone(),
        );
        match editor {
            Ok(editor) => return Box::new(editor),
            Err(e) => terminal::shell_warning(&format!(
                "ral: could not start the line editor ({e}); using the minimal one"
            )),
        }
    }
    Box::new(MinimalFrontend::new(history))
}
