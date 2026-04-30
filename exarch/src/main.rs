//! Exarch — a delegate that drives ral in process under a `grant`.
//!
//! Loops a chosen LLM provider against one tool — `shell` — that
//! evaluates ral source against a persistent `Shell`, each call wrapped
//! in a `grant` block.  Single-threaded REPL via rustyline.

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
use rustyline::error::ReadlineError;
use rustyline::{Editor, history::DefaultHistory};

fn main() -> std::process::ExitCode {
    // Hidden multicall dispatch — see
    // `ral_core::builtins::uutils::try_run_uutils_helper` for the rationale.
    // The parent ral/exarch process spawns `current_exe()` with the helper
    // sentinel as the first arg to run a bundled coreutils tool in a fresh
    // subprocess.  Not user-facing; not in `--help`.
    if let Some(code) = ral_core::builtins::uutils::try_run_uutils_helper() {
        return std::process::ExitCode::from(code);
    }
    if let Some(code) = runtime::sandbox_dispatch_or_continue() {
        return code;
    }
    // Undocumented debug switch — see `ral_core::sandbox::SANDBOX_DUMP_PROFILE_ENV`.
    // No-op unless the env var is set; when set, prints the OS-sandbox profile
    // exarch *would* install for an empty policy and continues startup.
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
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());
    let (caps, restrict_files) = policy::for_invocation(
        &cwd,
        &c.base,
        c.extend_base.as_deref(),
        &c.restrict,
    )?;
    // Allocate the per-session scratch dir before assembling the prompt
    // so its path can be named in the Grant section the agent reads.
    let scratch = runtime::Scratch::new().map_err(|e| format!("scratch dir: {e}"))?;
    let system = prompt::assemble(&c.system_files, &caps, scratch.path())?;
    let system_size = system.len();

    let mut provider = Provider::new(c.provider, key, model, system);
    let mut shell = runtime::boot_shell();
    scratch.install_into(&mut shell);
    // boot_shell installed ral's signal handlers; chain ours on top so
    // a single Ctrl-C unwinds both the in-flight ral evaluator and the
    // exarch turn loop.
    cancel::install();
    ui::banner(
        label,
        provider.model(),
        system_size,
        &c.system_files,
        &c.base,
        c.extend_base.as_deref(),
        &restrict_files,
        scratch.path(),
    );
    let spill = runtime::Spill::new().map_err(|e| format!("spill dir: {e}"))?;

    let mut ed: Editor<(), DefaultHistory> = Editor::new().map_err(|e| e.to_string())?;
    let mut total = Usage::default();
    let mut pending = seed.filter(|s| !s.trim().is_empty());

    loop {
        let prompt = if let Some(p) = pending.take() {
            p
        } else {
            match ed.readline("▸ ") {
                Ok(l) => {
                    let m = l.trim();
                    if m.is_empty() {
                        continue;
                    }
                    if matches!(m, "/quit" | "/exit") {
                        break;
                    }
                    if m == "/clear" {
                        provider.clear_history();
                        shell = runtime::boot_shell();
                        scratch.install_into(&mut shell);
                        total = Usage::default();
                        ui::banner(
                            label,
                            provider.model(),
                            system_size,
                            &c.system_files,
                            &c.base,
                            c.extend_base.as_deref(),
                            &restrict_files,
                            scratch.path(),
                        );
                        continue;
                    }
                    if m == "/compact" {
                        runtime::maybe_compact(&mut provider, &mut total);
                        continue;
                    }
                    let _ = ed.add_history_entry(m);
                    m.to_string()
                }
                Err(ReadlineError::Interrupted) => {
                    cancel::clear();
                    continue;
                }
                Err(ReadlineError::Eof) => break,
                Err(e) => return Err(e.to_string()),
            }
        };
        runtime::maybe_compact(&mut provider, &mut total);
        match runtime::run_task(&mut provider, &mut shell, &caps, &spill, &mut total, prompt) {
            Ok((task, hit_max_turns)) => {
                ui::cost_summary(&task, &total);
                if hit_max_turns {
                    pending = Some("[max turns reached — please continue where you left off]".into());
                }
            }
            Err(e) => ui::error(&e),
        }
    }
    Ok(())
}
