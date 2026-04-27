//! Exarch — a delegate that drives ral in process under a `grant`.
//!
//! Loops a chosen LLM provider against one tool — `shell` — that
//! evaluates ral source against a persistent `Shell`, each call wrapped
//! in a `grant` block.  Single-threaded REPL via rustyline.

mod api;
mod cli;
mod eval;
mod grant;
mod runtime;
mod ui;

use api::{Provider, Usage};
use clap::Parser;
use rustyline::error::ReadlineError;
use rustyline::{Editor, history::DefaultHistory};

fn main() -> std::process::ExitCode {
    if let Some(code) = runtime::sandbox_dispatch_or_continue() {
        return code;
    }
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
    let system = cli::load_system(&c.system_files)?;
    let key = std::env::var(key_env).map_err(|_| format!("{key_env} not set"))?;

    runtime::enable_os_sandbox();
    ui::banner(label, &model);

    let mut provider = Provider::new(c.provider, key, model, system);
    let mut shell = runtime::boot_shell();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());
    let spec = if std::env::var_os("EXARCH_DANGEROUS").is_some() {
        grant::GrantSpec::Dangerous
    } else {
        grant::GrantSpec::default_for(&cwd)
    };
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
                    if m == "/compact" {
                        runtime::maybe_compact(&mut provider, &mut total);
                        continue;
                    }
                    let _ = ed.add_history_entry(m);
                    m.to_string()
                }
                Err(ReadlineError::Interrupted) => continue,
                Err(ReadlineError::Eof) => break,
                Err(e) => return Err(e.to_string()),
            }
        };
        runtime::maybe_compact(&mut provider, &mut total);
        match runtime::run_task(&mut provider, &mut shell, &spec, &spill, &mut total, prompt) {
            Ok(task) => ui::cost_summary(&task, &total),
            Err(e) => ui::error(&e),
        }
    }
    Ok(())
}
