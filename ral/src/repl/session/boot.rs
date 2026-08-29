//! One-shot bootstrap for an interactive [`Session`](super::Session).
//!
//! The setup entrypoints here each run once at REPL startup, doing one
//! process-level setup task: signal handlers, terminal claim, panic
//! hook, terminal capability probe, profile/rc sourcing, frontend
//! construction.  (The startup-file helpers are the exception: the two
//! login profiles go through `source_profile` and the user rc through
//! `source_rc`, so their shared core runs up to three times.)  Splitting
//! them out of `session.rs` keeps the state machine itself focused on the
//! run/iterate/eval loop.

use ral_core::source::Span;
use ral_core::protocol::Program;
use ral_core::types::{Break, Closure, DefaultPolicy, Escape, HookName, HookSig, Map, Value};
use ral_core::{RequestedTerminalAccess, RunReport, Shell, diagnostic};
use std::sync::{Arc, Mutex};

use super::super::config::{RcSettings, apply_rc_config, create_default_rc, find_ralrc};
#[cfg(feature = "structural")]
use super::super::frontend::StructuralFrontend;
use super::super::frontend::{Frontend, MinimalFrontend, RustylineFrontend, Surface};
use super::super::plugin::{PluginRuntime, framed_run_request};

/// Install signal handlers and job-control signal masks for interactive use.
///
/// Unix disposition table:
/// - SIGINT  → relay handler (no-op when idle; forwards to external pipeline groups)
/// - SIGQUIT → quit handler (Ctrl+\ cancels the durable root — reaping the
///   foreground run and every detached worker — instead of core-dumping)
/// - SIGTERM/SIGHUP → term handler (cancels the durable root with `Terminate`;
///   the third delivery force-exits via the escalation ladder)
/// - SIGTSTP → `SIG_IGN`  (shell handles Ctrl+Z via waitpid, not self-stop)
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
            // SIGINT relay rather than SIG_IGN: a no-op when no relay slot is
            // active (the right behaviour between commands), forwarding to
            // external pipeline groups when one is.
            libc::signal(
                libc::SIGINT,
                ral_core::process::relay_handler() as *const () as libc::sighandler_t,
            );
            // Ctrl-\ cancels the durable root — the reap-everything gesture.
            libc::signal(
                libc::SIGQUIT,
                ral_core::process::quit_handler() as *const () as libc::sighandler_t,
            );
            let term = ral_core::process::term_handler() as *const () as libc::sighandler_t;
            libc::signal(libc::SIGTERM, term);
            libc::signal(libc::SIGHUP, term);
            // Ignore SIGTSTP (Ctrl+Z handled via waitpid) and SIGTTOU/SIGTTIN
            // so the shell manipulates the terminal and reads stdin without
            // being stopped when backgrounded.
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
    reason = "[io-door:silent:crashlog-write] panic hook creates the state dir and writes a crash log; not turn-time model I/O"
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
    let hook_name = HookName::session("prompt");
    if shell.has_hook(&hook_name) {
        return;
    }
    // Build the return-the-constant thunk directly rather than interpolating
    // `DEFAULT_PROMPT` into ral source and compiling it: the prompt value is
    // then decoupled from what the constant's bytes happen to be, so no
    // boot-time `.expect` can panic on its contents.
    let block = Value::Thunk(Closure {
        comp: Arc::new(ral_core::source::Spanned::synthetic(
            ral_core::ir::CompKind::Return(ral_core::ir::Val::String(
                super::super::prompt::DEFAULT_PROMPT.into(),
            )),
        )),
        env: ral_core::types::Env::default(),
    });
    let origin = ral_core::source::Span::synthetic();
    let _ = shell.register_hook(
        hook_name,
        block,
        HookSig::Prompt,
        DefaultPolicy::denied_capture(),
        origin,
    );
}

/// Mark the shell interactive and publish the probed terminal as the
/// `TERMINAL` scope binding.  The terminal itself was probed before the
/// shell was constructed and is exposed via `shell.terminal()`.
pub(super) fn setup_terminal(shell: &mut Shell) {
    shell.set_interactive(true);
    let terminal = shell.terminal().to_value();
    shell.set_var("TERMINAL".into(), terminal);
}

/// Source login profiles (if login shell) and the user RC file, returning
/// the frontend settings the rc resolved (the defaults when no rc ran).
///
/// Login profiles: `/etc/ral/profile`, then `~/.ral_profile` — scripts
/// sourced for their effects, contracted to return `()`.
/// RC: `$XDG_CONFIG_HOME/ral/rc` or `~/.ralrc` (created from a default
/// skeleton if neither exists) — a configuration expression, contracted
/// to return the map that [`apply_rc_config`] applies.
///
/// `no_rc` (`--norc`/`--noprofile`) suppresses every startup file — the
/// login profiles as well as the RC file — so a `-l --norc` session boots
/// from a clean slate.
pub(super) fn load_profiles(
    is_login: bool,
    no_rc: bool,
    shell: &mut Shell,
    runtime: &Arc<Mutex<PluginRuntime>>,
) -> RcSettings {
    if is_login && !no_rc {
        let system_profile = "/etc/ral/profile".to_string();
        let user_profile = ral_core::path::config::home_dot(".ral_profile")
            .map(|p| p.to_string_lossy().into_owned());
        for path in [Some(system_profile), user_profile].into_iter().flatten() {
            if ral_core::path::exists(&path) {
                source_profile(&path, shell);
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
            return source_rc(&rc_path, shell, runtime);
        }
    }
    RcSettings::default()
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
/// silently.  The readline frontend routes background `watch` output above
/// the prompt itself (see [`RustylineFrontend::new`]).
pub(super) fn create_frontend(
    interactive_mode: ral_core::io::InteractiveMode,
    settings: RcSettings,
    shell: &mut Shell,
    runtime: Arc<Mutex<PluginRuntime>>,
) -> Box<dyn Frontend> {
    if matches!(interactive_mode, ral_core::io::InteractiveMode::Minimal) {
        return Box::new(MinimalFrontend::new());
    }
    match settings.surface {
        Surface::Minimal => return Box::new(MinimalFrontend::new()),
        // The structural surface needs raw mode; its `new` probes for it and
        // errors when unavailable, so a failure warns and falls through.
        Surface::Structural => {
            #[cfg(feature = "structural")]
            match StructuralFrontend::new(settings.edit_mode, runtime.clone()) {
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
    Box::new(RustylineFrontend::new(
        shell,
        settings.edit_mode,
        settings.bell,
        runtime,
    ))
}

// ── Startup-file sourcing ───────────────────────────────────────────────
//
// Two kinds of startup file, each with exactly one contract on its return
// value: a login profile is a script sourced for its effects and must
// return `()`; the rc file is a configuration expression and must
// return a map.  Both share [`evaluate_startup_file`].

/// Source a login profile.  Anything other than `()` — a configuration
/// map included — is rejected: silently discarding a returned map would
/// let a misplaced `[theme: …]` in `~/.ral_profile` do nothing without a
/// word, and configuration belongs in the rc file.  A profile only ever
/// mutates the shell, so it yields no [`RcSettings`].
fn source_profile(path: &str, shell: &mut Shell) {
    if let Err(msg) = source_profile_inner(path, shell) {
        diagnostic::cmd_error("ral", &msg);
    }
}

fn source_profile_inner(path: &str, shell: &mut Shell) -> Result<(), String> {
    match evaluate_startup_file(path, shell)? {
        None | Some(Value::Unit) => Ok(()),
        Some(v) => Err(format!(
            "{path}: profile must return (); got {} — configuration belongs in the rc file",
            v.type_name()
        )),
    }
}

/// Source the rc file: evaluate it, apply the returned map, and run its
/// `startup` block if present.  Each stage's failure is reported where
/// the right amount of state survives it — a broken file or contract
/// leaves the default settings; a failed `startup` block keeps the
/// settings the map already applied.
fn source_rc(path: &str, shell: &mut Shell, runtime: &Arc<Mutex<PluginRuntime>>) -> RcSettings {
    let pairs = match rc_config(path, shell) {
        Ok(Some(pairs)) => pairs,
        Ok(None) => return RcSettings::default(),
        Err(msg) => {
            diagnostic::cmd_error("ral", &msg);
            return RcSettings::default();
        }
    };
    let (settings, startup) = apply_rc_config(pairs, shell, runtime);
    if let Some(block) = startup
        && let Err(msg) = run_startup(path, block, shell)
    {
        diagnostic::cmd_error("ral", &msg);
    }
    settings
}

/// Evaluate the rc file and check its contract: a configuration map.
/// `Ok(None)` when `exit` escaped the file before it produced one.
fn rc_config(path: &str, shell: &mut Shell) -> Result<Option<Map>, String> {
    match evaluate_startup_file(path, shell)? {
        None => Ok(None),
        Some(Value::Map(pairs)) => Ok(Some(pairs)),
        Some(other) => Err(format!(
            "{path}: rc file must return a map; got {}",
            other.type_name()
        )),
    }
}

/// Register the rc `startup` block as the session `startup` hook and run
/// it once, terminal-denied.
fn run_startup(path: &str, block: Value, shell: &mut Shell) -> Result<(), String> {
    let origin = Span::synthetic();
    if let Err(e) = shell.register_hook(
        HookName::session("startup"),
        block,
        HookSig::Prompt,
        DefaultPolicy::denied(),
        origin,
    ) {
        return Err(format!("{path}: startup: {e}"));
    }
    let req = framed_run_request(
        "<startup>",
        RequestedTerminalAccess::Denied,
        Program::Hook {
            name: HookName::session("startup"),
            args: vec![],
        },
    );
    match shell.run(req) {
        RunReport::Ran { ending, .. } => match ending.into_result() {
            Ok(_) | Err(Break::Escape(Escape::Exit(_))) => Ok(()),
            Err(Break::Error(e)) => Err(format!("{path}: startup: {}", e.message)),
            #[cfg(unix)]
            Err(Break::Escape(Escape::Stopped { .. })) => Err(format!(
                "{path}: startup: a stop signal escaped — rc files cannot park jobs"
            )),
        },
        RunReport::Static { .. } => {
            unreachable!("a thunk startup block never compiles source")
        }
    }
}

/// The shared core: read, typecheck, and evaluate one startup file,
/// returning its value for the caller's contract check.  `Ok(None)` means
/// `exit` escaped the file — sourcing stops with no value to check.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:config-read] reads an rc/profile file during session boot; not turn-time model I/O"
)]
fn evaluate_startup_file(path: &str, shell: &mut Shell) -> Result<Option<Value>, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let src = ral_core::source::normalize_source_text(src);
    // The check always runs (it writes the annotations the evaluator reads —
    // capture points and each pipeline's final route) and is seeded from the
    // live shell, so an earlier startup file's bindings are visible to a
    // later file's check.  A type error of any kind leaves the file with no
    // runnable annotation: it is reported and skipped while the boot
    // survives — a broken startup file must not strand the user at no shell,
    // the parse-error precedent above.
    //
    // Compiled against the `FileId` `evaluate_checked` registers the text
    // under a moment later, exactly as a module load is: an alias or function
    // the file defines outlives this boot, and its spans have to keep naming
    // the file for the whole session.
    let file = shell.sources().next_id();
    let comp = match ral_core::compile_and_typecheck(&src, shell.session_schemes(), file, path) {
        ral_core::CompileOutcome::Compiled(annotated) => std::sync::Arc::new(annotated),
        ral_core::CompileOutcome::Parse(e) => return Err(format!("{path}: {e}")),
        ral_core::CompileOutcome::Types(errs) => {
            eprint!(
                "{}",
                diagnostic::format_type_errors_ariadne(path, &src, &errs)
            );
            return Err(format!("{path}: skipped due to type errors"));
        }
    };
    // Evaluate under the same guarded pipeline `source`/`use`/plugin loading
    // share: `evaluate_checked` owns the cycle and depth guards, and registers
    // the text under the id compiled against just above, so a runtime error
    // inside the file is located against it.
    match ral_core::builtins::modules::evaluate_checked(
        &ral_core::types::Mooring::adrift(),
        shell,
        &comp,
        &src,
        path,
    ) {
        Ok(v) => Ok(Some(v)),
        Err(Break::Error(e)) => Err(format!("{path}: {}", e.message)),
        // `exit` in a startup file: stop sourcing it, boot continues.
        Err(Break::Escape(Escape::Exit(_))) => Ok(None),
        // A stop signal (Ctrl-Z during a slow startup command) parks a
        // process group the REPL job table never learned about — never
        // resumed, never reaped.  Report it like the `startup:` arm in
        // `source_rc_inner` rather than silently orphaning the group.
        #[cfg(unix)]
        Err(Break::Escape(Escape::Stopped { .. })) => Err(format!(
            "{path}: a stop signal escaped — startup files cannot park jobs"
        )),
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs scaffolding"
)]
mod tests {
    use super::*;

    /// Write `src` as a startup file, returning the tempdir (keeping the
    /// file alive) and its path.
    fn startup_file(src: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("startup.ral");
        std::fs::write(&path, src).unwrap();
        let path = path.to_str().unwrap().to_owned();
        (dir, path)
    }

    fn booted_shell() -> Shell {
        ral_core::boot::boot_shell(
            ral_core::io::TerminalState::default(),
            &crate::PRELUDE,
            &ral_core::HostSurface::default(),
        )
    }

    /// A side-effect-only profile returning `()` sources cleanly.
    #[test]
    fn profile_returning_unit_sources_cleanly() {
        let (_dir, path) = startup_file("return ()\n");
        assert_eq!(source_profile_inner(&path, &mut booted_shell()), Ok(()));
    }

    /// A profile returning a configuration map is rejected — the error
    /// names the profile contract and points at the rc file.
    #[test]
    fn profile_returning_map_is_rejected() {
        let (_dir, path) = startup_file("return [edit_mode: 'vi']\n");
        let err = source_profile_inner(&path, &mut booted_shell()).unwrap_err();
        assert!(err.contains("profile must return (); got Map"), "{err}");
        assert!(
            err.contains("configuration belongs in the rc file"),
            "{err}"
        );
    }

    /// An rc file returning `()` is rejected — the error names the rc
    /// contract.
    #[test]
    fn rc_returning_unit_is_rejected() {
        let (_dir, path) = startup_file("return ()\n");
        let err = rc_config(&path, &mut booted_shell()).unwrap_err();
        assert!(err.contains("rc file must return a map; got Unit"), "{err}");
    }
}
