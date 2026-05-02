//! Exarch — a delegate that drives ral in process under a `grant`.
//!
//! Loops a chosen LLM provider against one tool — `shell` — that
//! evaluates ral source against a persistent `Shell`, each call wrapped
//! in a `grant` block.  Single-threaded REPL on top of a ratatui
//! inline-viewport TUI: each turn runs on a scoped worker thread that
//! emits `UiEvent`s through a channel; the main thread owns the
//! terminal and pumps events while keeping the input editor live, so
//! the user can compose the next prompt during a turn without echo
//! artefacts.

mod api;
mod cancel;
mod cli;
mod eval;
mod host;
mod policy;
mod prompt;
mod runtime;
mod ui;

use api::{Provider, Usage};
use clap::Parser;
use ratatui::crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use std::io;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::Duration;
use ui::{App, Term, UiEvent};

fn main() -> std::process::ExitCode {
    if let Some(code) = ral_core::builtins::uutils::try_run_uutils_helper() {
        return std::process::ExitCode::from(code);
    }
    if let Some(code) = runtime::sandbox_dispatch_or_continue() {
        return code;
    }
    ral_core::sandbox::dump_profile_if_requested(&ral_core::types::SandboxProjection::default());
    match real_main() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("exarch: {e}");
            std::process::ExitCode::from(1)
        }
    }
}

fn real_main() -> Result<(), String> {
    let c = cli::Cli::parse();
    let seed = cli::load_seed(c.prompt, c.file)?;
    let (label, default_model, key_env, _) = c.provider.info();
    let model = c.model.unwrap_or_else(|| default_model.into());
    let key = std::env::var(key_env).map_err(|_| format!("{key_env} not set"))?;
    #[allow(clippy::disallowed_methods)]
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());
    let (caps, restrict_files) = policy::for_invocation(
        &cwd, &c.base, c.extend_base.as_deref(), &c.restrict,
    )?;
    let scratch = runtime::Scratch::new().map_err(|e| format!("scratch dir: {e}"))?;
    let system = prompt::assemble(&c.system_files, &caps, scratch.path())?;
    let system_size = system.len();

    let mut provider = Provider::new(c.provider, key, model, system);
    let mut shell = runtime::boot_shell();
    scratch.install_into(&mut shell);
    cancel::install();
    let spill = runtime::Spill::new().map_err(|e| format!("spill dir: {e}"))?;

    let mut term = ui::enter().map_err(|e| format!("ratatui init: {e}"))?;
    let mut app = App::new();
    let r = drive(
        &mut term, &mut app, &mut provider, &mut shell, &caps, &spill,
        label, system_size, &c.system_files, &c.base, c.extend_base.as_deref(),
        &restrict_files, &scratch, seed,
    );
    ui::leave(&mut term);
    r
}

#[allow(clippy::too_many_arguments)]
fn drive(
    term: &mut Term,
    app: &mut App,
    provider: &mut Provider,
    shell: &mut ral_core::Shell,
    caps: &ral_core::types::Capabilities,
    spill: &runtime::Spill,
    label: &str,
    system_size: usize,
    system_files: &[std::path::PathBuf],
    base: &str,
    extend_base: Option<&std::path::Path>,
    restrict_files: &[std::path::PathBuf],
    scratch: &runtime::Scratch,
    seed: Option<String>,
) -> Result<(), String> {
    app.banner(
        term, label, provider.model(), system_size, system_files,
        base, extend_base, restrict_files, scratch.path(),
    ).map_err(|e| e.to_string())?;

    let mut total = Usage::default();
    let mut pending = seed.filter(|s| !s.trim().is_empty());

    loop {
        let prompt = if let Some(p) = pending.take() {
            p
        } else {
            match read_prompt(term, app).map_err(|e| e.to_string())? {
                Some(s) => s,
                None => return Ok(()),
            }
        };
        let trimmed = prompt.trim();
        if trimmed.is_empty() { continue; }
        if matches!(trimmed, "/quit" | "/exit") { return Ok(()); }
        if trimmed == "/clear" {
            provider.clear_history();
            *shell = runtime::boot_shell();
            scratch.install_into(shell);
            total = Usage::default();
            app.handle(term, UiEvent::Cost(0.0)).map_err(|e| e.to_string())?;
            app.banner(
                term, label, provider.model(), system_size, system_files,
                base, extend_base, restrict_files, scratch.path(),
            ).map_err(|e| e.to_string())?;
            continue;
        }
        if trimmed == "/compact" {
            pump(term, app, |tx| runtime::maybe_compact(provider, &mut total, tx))
                .map_err(|e| e.to_string())?;
            continue;
        }

        pump(term, app, |tx| runtime::maybe_compact(provider, &mut total, tx))
            .map_err(|e| e.to_string())?;
        // Echo the submitted prompt above the inline viewport so the
        // user sees what was sent before the turn header arrives.
        ui::insert_lines(term, ui::user_prompt(&prompt)).map_err(|e| e.to_string())?;
        let result = pump(term, app, |tx| {
            runtime::run_task(provider, shell, caps, spill, &mut total, prompt, tx)
        }).map_err(|e| e.to_string())?;
        match result {
            Ok((task, hit_max_turns)) => {
                let _ = ui::insert_lines(term, ui::cost_summary(&task, &total));
                if hit_max_turns {
                    pending = Some("[max turns reached — please continue where you left off]".into());
                }
            }
            Err(e) => {
                let _ = ui::insert_lines(term, ui::error(&e));
            }
        }
    }
}

/// Run `work` on a scoped worker thread while the main thread pumps
/// UI events from the channel until the worker drops its sender.
/// The worker's return value is propagated back to the caller, so
/// the same shape covers compaction (`R = ()`) and a full turn (`R =
/// Result<(Usage, bool), String>`) without two near-identical
/// helpers.  Toggles the App's busy spinner around the work for free.
fn pump<R: Send>(
    term: &mut Term,
    app: &mut App,
    work: impl Send + FnOnce(&Sender<UiEvent>) -> R,
) -> io::Result<R> {
    app.busy_on();
    let r = std::thread::scope(|s| -> io::Result<R> {
        let (tx, rx) = channel();
        let h = s.spawn(move || {
            let r = work(&tx);
            drop(tx); // signal Disconnected to the pump loop
            r
        });
        drive_events(term, app, rx)?;
        Ok(h.join().expect("worker panicked"))
    });
    app.busy_off();
    r
}

/// Pump UI events from `rx` plus key events from the terminal until
/// `rx` disconnects (worker dropped its sender).  Key events are
/// routed into the input editor — the user composes the *next*
/// prompt during the turn — and Ctrl-C raises the cancel flag.
/// Enter is *not* a submit during a turn; submission happens once
/// control returns to `read_prompt`.
fn drive_events(
    term: &mut Term,
    app: &mut App,
    rx: Receiver<UiEvent>,
) -> io::Result<()> {
    loop {
        // Drain pending events without blocking.
        loop {
            match rx.try_recv() {
                Ok(ev) => app.handle(term, ev)?,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    app.draw(term)?;
                    return Ok(());
                }
            }
        }
        app.draw(term)?;
        if event::poll(Duration::from_millis(50))? {
            if let CtEvent::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press { continue; }
                if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                    cancel::raise();
                    continue;
                }
                app.key(k);
            }
        }
    }
}

/// Idle event loop: render the empty viewport with the input editor,
/// route key events into the editor, and return when the user
/// submits a non-empty line (Some) or asks to exit (None) via
/// Ctrl-D, or Ctrl-C when the editor is empty.
fn read_prompt(term: &mut Term, app: &mut App) -> io::Result<Option<String>> {
    loop {
        app.draw(term)?;
        if !event::poll(Duration::from_millis(100))? { continue; }
        let CtEvent::Key(k) = event::read()? else { continue };
        if k.kind != KeyEventKind::Press { continue; }
        match (k.code, k.modifiers) {
            (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => return Ok(None),
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                if app.submit().is_some() {
                    // submit() takes + clears; we throw it away — Ctrl-C
                    // is "abandon current line".
                } else {
                    return Ok(None);
                }
            }
            (KeyCode::Enter, m) if !m.contains(KeyModifiers::SHIFT) && !m.contains(KeyModifiers::ALT) => {
                if let Some(s) = app.submit() {
                    return Ok(Some(s));
                }
            }
            _ => app.key(k),
        }
    }
}
