//! A coding agent based on the ral shell, driven in process under a
//! user-chosen grant policy.
//!
//! The whole agent is here: the CLI, capability composition, the
//! [`agent::Avatar`] exchange driver, the [`provider::Provider`] transport, and
//! the two frontends ([`tui::run`] / [`headless::run`]).  The `exarch` binary is
//! a thin shell over [`run`]; integration tests link this library directly.
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_macros,
        reason = "[test] libtest captures print macros"
    )
)]
#![allow(
    clippy::disallowed_methods,
    reason = "exarch is an application, not the ral shell; the clippy.toml invariants target ral-core's Shell path/cwd/fs discipline"
)]
pub mod agent;
pub mod app;
pub mod boot;
pub mod bus;
pub mod cancel;
pub mod card;
pub mod cli;
pub mod clock;
pub mod config;
pub mod egress;
pub mod enquiry;
pub mod headless;
pub(crate) mod latch;
pub mod library;
pub mod net_policy;
pub mod policy;
pub mod prompt;
pub mod provider;
pub mod record;
pub mod schedule;
pub mod shell_eval;
pub(crate) mod signals;
pub mod skill;
pub mod tui;

use agent::Avatar;
use clap::Parser;
use provider::state::{Opening, Pair, opening};
use provider::{Bureau, Engine, Holdings};
use std::sync::Arc;

/// The one boot recipe table, for the identity seat and the `--engine` child
/// alike.
pub static INSTALLERS: [ral_core::engine::EngineInstaller; 1] =
    [ral_core::engine::EngineInstaller {
        tag: shell_eval::builtins::INSTALLER_TAG,
        boot: boot::engine_boot_shell,
        narrow: policy::base_layer,
    }];

/// The full pre-`main` dispatch, shared by the binary's `main` and every test
/// `#[ctor]`: core's [`serve_process`](ral_core::invocation::serve_process) over
/// exarch's [`INSTALLERS`].
///
/// The pipeline anchor re-execs the running binary, which under `cargo test`
/// is the libtest harness, so the flag must be served before libtest sees argv
/// and rejects it.
///
/// `Some(code)` means this process is a re-exec child that should exit now.
pub fn dispatch_pre_main() -> Option<u8> {
    ral_core::invocation::serve_process(&INSTALLERS)
}

/// [`dispatch_pre_main`], and the exit its answer calls for — one expression,
/// so the two `main`s over this crate and every [`pre_main_ctor!`] cannot drift
/// on what makes exiting here safe.
///
/// Returns only when this process is no such child, so a `main` calls it as its
/// first real statement and a `#[ctor]` calls it as its whole body.
pub fn exit_if_re_exec_child() {
    if let Some(code) = dispatch_pre_main() {
        #[allow(
            clippy::disallowed_methods,
            reason = "a re-exec child, served before any CLI: the anchor drains a pipe, the pgid probe writes a line, the sandbox stage has already exec'd or refused. None boots a shell, so none holds a lease, a watched child or a staged write; and in the `#[ctor]` form nothing at all is booted yet"
        )]
        std::process::exit(i32::from(code));
    }
}

/// Emit the `#[ctor]` running [`exit_if_re_exec_child`].
///
/// A re-exec child is then gone before libtest sees flags it would reject.
/// Once per binary; gate with `#[cfg(test)]` where only the test build wants it.
#[macro_export]
macro_rules! pre_main_ctor {
    () => {
        #[ctor::ctor(unsafe)]
        fn init_pre_main() {
            $crate::exit_if_re_exec_child();
        }
    };
}

#[cfg(test)]
pre_main_ctor!();

/// Metadata shown in the startup banner.
pub struct SessionInfo<'a> {
    pub system_size: usize,
    pub system_files: &'a [std::path::PathBuf],
    pub base: &'a str,
    pub extend_base: Option<&'a std::path::Path>,
    pub restrict_files: &'a [std::path::PathBuf],
    pub cwd: &'a str,
    /// What a `--resume` picked up, `None` for a fresh session.
    pub resumed: Option<crate::record::Resumed>,
}

/// The binary's entry point, lifted into the library so integration tests can
/// link the whole crate.
///
/// It parses the CLI, composes the capability lattice, builds an [`Avatar`] +
/// [`provider::Provider`], and hands off to a frontend.
///
/// # Errors
/// Returns `Err` if the CLI is misused, if no provider is available, or if
/// loading the provider config, resolving the model selection, building the
/// capability policy, setting up the scratch/log directories, or the chosen
/// frontend fails.
pub fn run() -> Result<(), String> {
    let c = cli::Cli::parse();
    let headless = c.is_headless();
    // Subcommands act and exit before the provider-availability check below,
    // since `login` is how the OpenAI provider becomes available.
    if let Some(command) = c.command {
        return match command {
            cli::Command::Login { account } => provider::oauth::login(account),
            cli::Command::Logout { account, all } => provider::oauth::logout(account, all),
            cli::Command::Accounts => {
                let accounts = provider::oauth::accounts();
                if accounts.is_empty() {
                    ral_core::errln!(
                        "No ChatGPT accounts signed in. Run `exarch login` to add one."
                    );
                } else {
                    for account in &accounts {
                        ral_core::outln!("{}", provider::identity::label(account, &accounts));
                    }
                }
                Ok(())
            }
        };
    }
    let seed = cli::load_seed(c.prompt, c.file)?;

    let custom = config::load()?;
    let disk_warn_bytes = config::disk_warn_bytes()?;
    // Opened once at the trunk; every spawned child inherits this ledger.
    let egress = egress::Egress::open(app::EXARCH)?;

    // SAFETY: startup is still single-threaded — the tokio runtime and the
    // session's workers come later — so nothing races this env mutation.  It is
    // the only scrub, so every child inherits an environment free of keys.
    let store = provider::credential::CredentialStore::resolve_and_scrub(custom);
    let available = store.available();
    if available.is_empty() {
        return Err(
            "no provider available: set a provider API key (e.g. ANTHROPIC_API_KEY, \
             OPENAI_API_KEY, OPENROUTER_API_KEY, DEEPSEEK_API_KEY)"
                .into(),
        );
    }

    let cwd = std::env::current_dir()
        .map_err(|e| format!("launch cwd: {e}"))?
        .to_string_lossy()
        .into_owned();
    let state_dir = app::EXARCH.project_dir(&cwd);

    let holdings = Holdings::new(store, app::EXARCH);
    let saved = provider::state::load(&state_dir);
    let mut tuning = provider::Tuning::initial();
    if let Some(rung) = c.effort.as_deref() {
        tuning.effort = provider::effort_by_label(rung)?;
    }
    let opening = opening(
        c.provider.as_deref(),
        c.model.as_deref(),
        saved.as_ref(),
        &available,
        |account| provider::models::listing_of(&holdings.catalog, &account.id),
    )?;

    let (caps, restrict_files) =
        policy::for_invocation(&cwd, &c.base, c.extend_base.as_deref(), &c.restrict)?;
    let scratch =
        Arc::new(app::Scratch::new(app::EXARCH).map_err(|e| format!("scratch dir: {e}"))?);
    let (_mode, terminal, _warn) = ral_core::terminal::TerminalState::probe_from_env();
    boot::face_process_signals(&terminal);
    // Held for the whole run: the lock is the launcher's, not the node's.
    let (run_dir, _run_lock, resumes) = resolve_run(&cwd, c.resume)?;
    let config_dir = app::EXARCH.xdg_dir(ral_core::host::XdgKind::Config);
    let cwd_path = std::path::PathBuf::from(&cwd);
    // Chat registers no tools, so there is nothing for a system prompt to say.
    let system = if c.chat {
        prompt::CHAT_SYSTEM.to_string()
    } else {
        prompt::assemble(
            &c.system_files,
            &caps,
            app::EXARCH,
            &cwd_path,
            &config_dir,
            !headless,
            c.edit,
        )?
    };
    let system_size = system.len();

    // One runtime for the whole fleet; per-credential transports warm lazily.
    let engine = Engine::new();
    let bureau = Arc::new(Bureau::Live { engine, holdings });
    let (provider, screen) = open(
        opening,
        &tuning,
        (!headless).then_some(run_dir.as_path()),
        &bureau,
        c.max_tokens,
    )?;
    // The selection a launch opens with is the one the next launch restores.
    let _ = provider::state::save(
        &state_dir,
        &provider::state::State::of(&provider, &available),
    );
    let config = agent::RootConfig {
        system,
        caps,
        run_dir: run_dir.clone(),
        account: record::RecordedAccount::of(provider.account(), &available),
        // An attended trunk parks for the human; a headless one terminates
        // once its seeded work is idle.
        trunk: if headless {
            agent::Trunk::Headless
        } else {
            agent::Trunk::Attended
        },
        tools: if c.chat {
            provider::Toolset::default()
        } else {
            provider::Toolset::offered(c.thinking_tool)
        },
        allow_schedule: c.allow_schedule,
        resume_on_reset: !headless,
        disk_warn_bytes,
        fuel: agent::SPAWN_FUEL,
        egress,
        dial: None,
        bureau: Arc::clone(&bureau),
    };
    let seat = agent::RootSeat::Identity {
        scratch,
        cwd: cwd_path,
        terminal,
    };
    let (mut session, resumed) = if resumes {
        Avatar::resume(config, seat, Arc::clone(&provider)).map(|(s, r)| (s, Some(r)))
    } else {
        Avatar::root(config, seat, Arc::clone(&provider)).map(|s| (s, None))
    }
    .map_err(|e| format!("session init: {e}"))?;

    let info = SessionInfo {
        system_size,
        system_files: &c.system_files,
        base: &c.base,
        extend_base: c.extend_base.as_deref(),
        restrict_files: &restrict_files,
        cwd: &cwd,
        resumed,
    };
    if headless {
        headless::run(&mut session, &info, &provider, seed, c.output_format)
    } else {
        let screen = match screen {
            Some(screen) => screen,
            None => tui::enter(&run_dir)?,
        };
        tui::run(screen, &mut session, &provider, &info, &bureau, seed, c.vi)
    }
}

/// Resolve a fresh or resumable run directory, retaining the lock for the
/// process that owns it; the flag says whether session 0 is to be resumed
/// from it.
#[allow(
    clippy::option_option,
    reason = "the CLI distinguishes absent, bare, and named resume"
)]
fn resolve_run(
    cwd: &str,
    resume: Option<Option<std::path::PathBuf>>,
) -> Result<(std::path::PathBuf, app::RunLock, bool), String> {
    if let Some(target) = resume {
        let explicit = target.is_some();
        let candidates = match target {
            Some(target) => vec![app::normalize_resume_target(&target)?],
            None => app::EXARCH
                .resume_candidates(cwd)
                .map_err(|error| format!("could not inspect resumable runs: {error}"))?,
        };
        for run_dir in candidates {
            let record = run_dir.join("sessions/0/record.jsonl");
            if !record.is_file() {
                if explicit {
                    return Err(format!(
                        "--resume target {} has no {}; pass a run directory containing sessions/0/record.jsonl",
                        run_dir.display(),
                        record.display()
                    ));
                }
                continue;
            }
            match app::RunLock::try_acquire(&run_dir) {
                Ok(lock) => return Ok((run_dir, lock, true)),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if explicit {
                        return Err(format!(
                            "--resume target {} is already running; run.lock is held by another exarch",
                            run_dir.display()
                        ));
                    }
                }
                Err(error) => {
                    return Err(format!(
                        "cannot resume {}: could not acquire {}: {error}",
                        run_dir.display(),
                        run_dir.join("run.lock").display()
                    ));
                }
            }
        }
        return Err(format!(
            "--resume found no unlocked run with sessions/0/record.jsonl under {}",
            app::EXARCH.project_dir(cwd).display()
        ));
    }

    let run_dir = app::EXARCH
        .log_run_dir(cwd)
        .map_err(|error| format!("log dir: {error}"))?;
    let lock = app::RunLock::try_acquire(&run_dir)
        .map_err(|error| format!("could not lock {}: {error}", run_dir.display()))?;
    Ok((run_dir, lock, false))
}

/// The provider a launch opens with: `opening` built, or — when it names no
/// pair, or a remembered one no longer stands — one chosen on a screen taken
/// in `attended`, the run directory of a launch with a human at it. That
/// screen is handed back for the session to keep.
///
/// # Errors
/// If a named pair does not build, if a choice is needed with no human to
/// make it, or if the user backs out of the picker.
fn open(
    opening: Opening,
    tuning: &provider::Tuning,
    attended: Option<&std::path::Path>,
    bureau: &Bureau,
    max_tokens: Option<u32>,
) -> Result<(Arc<provider::Provider>, Option<tui::TerminalGuard>), String> {
    let build = |pair: Pair| bureau.build(&pair.account, pair.model, tuning, None, max_tokens);
    let choose = |among, why: Option<String>| {
        let Some(run_dir) = attended else {
            return Err(format!(
                "{}: pass --model NAME for a headless run",
                why.as_deref().unwrap_or("no model chosen")
            ));
        };
        let mut screen = tui::enter(run_dir)?;
        let pick = tui::choose(&mut screen, bureau, among, tuning, why)?;
        let provider = bureau.build(
            &pick.account,
            pick.model,
            &pick.tuning,
            pick.route,
            max_tokens,
        )?;
        Ok((provider, Some(screen)))
    };
    match opening {
        Opening::Named(pair) => Ok((build(pair)?, None)),
        Opening::Restored(pair, among) => match build(pair) {
            Ok(provider) => Ok((provider, None)),
            Err(why) => choose(
                among,
                Some(format!("could not restore the saved model: {why}")),
            ),
        },
        Opening::Choose(among, why) => choose(among, why),
    }
}
