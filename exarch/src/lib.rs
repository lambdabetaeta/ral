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
#[cfg(all(unix, test))]
pub(crate) mod test_helper;
pub mod tui;
pub mod wallet;

use agent::Avatar;
use clap::Parser;
use provider::state::{Opening, opening};
use provider::{Bureau, Choice, Engine, Holdings};
use std::sync::Arc;

/// The one boot recipe table, for the identity seat and the `--engine` child
/// alike.
pub static INSTALLERS: [ral_core::engine::EngineInstaller; 1] =
    [ral_core::engine::EngineInstaller {
        tag: shell_eval::builtins::INSTALLER_TAG,
        runtime: boot::engine_runtime,
        root: boot::engine_root,
        narrow: policy::base_layer,
    }];

/// The exit a served re-exec role calls for, or a return when there is none.
///
/// `Some(code)` means this process is a re-exec child that should exit now.
/// The one place either entry point below exits, so the two `main`s over this
/// crate and every [`pre_main_ctor!`] cannot drift on what makes exiting here
/// safe.
fn exit_on(served: Option<u8>) {
    if let Some(code) = served {
        #[allow(
            clippy::disallowed_methods,
            reason = "a re-exec child, served before any CLI: the anchor drains a pipe, the sandbox stage has already exec'd or refused. None boots a shell, so none holds a lease, a watched child or a staged write; and in the `#[ctor]` form nothing at all is booted yet"
        )]
        std::process::exit(i32::from(code));
    }
}

/// The full pre-`main` dispatch for a `main`: core's
/// [`serve_process`](ral_core::invocation::serve_process) over exarch's
/// [`INSTALLERS`], and the exit its answer calls for.
///
/// Returns only when this process is no re-exec child, so a `main` calls it as
/// its first real statement.  A `#[ctor]` calls
/// [`exit_if_re_exec_child_before_main`] instead, which differs only in where
/// it reads argv.
pub fn exit_if_re_exec_child() {
    exit_on(ral_core::invocation::serve_process(&INSTALLERS));
}

/// [`exit_if_re_exec_child`] for a `#[ctor]`: core's
/// [`serve_process_before_main`](ral_core::invocation::serve_process_before_main)
/// over the same [`INSTALLERS`], and the same exit.
///
/// The pipeline anchor re-execs the running binary, which under `cargo test` is
/// the libtest harness, so its flag must be served before libtest sees argv and
/// rejects it.  A constructor runs before `main`, where std has argv on some
/// platforms and not others, hence the separate form; it is the body of every
/// [`pre_main_ctor!`] and not otherwise called.
#[doc(hidden)]
pub fn exit_if_re_exec_child_before_main() {
    exit_on(ral_core::invocation::serve_process_before_main(&INSTALLERS));
}

/// Emit the `#[ctor]` running [`exit_if_re_exec_child_before_main`].
///
/// A re-exec child is then gone before libtest sees flags it would reject.
/// Once per binary; gate with `#[cfg(test)]` where only the test build wants it.
#[macro_export]
macro_rules! pre_main_ctor {
    () => {
        #[ctor::ctor(unsafe)]
        fn init_pre_main() {
            $crate::exit_if_re_exec_child_before_main();
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
    pub extend: Option<&'a std::path::Path>,
    pub restrict_files: &'a [std::path::PathBuf],
    pub cwd: &'a str,
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
            cli::Command::Export { session, out } => {
                let record = if session.is_dir() {
                    session.join("record.jsonl")
                } else {
                    session
                };
                if !record.is_file() {
                    return Err(format!(
                        "no record.jsonl at {}; is this a session directory under $XDG_STATE_HOME/exarch?",
                        record.display()
                    ));
                }
                match &out {
                    Some(dest) => tui::export_to(&record, dest, tui::Format::Plain),
                    None => tui::export(&record, &mut std::io::stdout().lock(), tui::Format::Plain),
                }
                .map_err(|e| format!("could not export {}: {e}", record.display()))
            }
        };
    }
    let seed = cli::load_seed(c.prompt, c.file)?;

    let wallet = wallet::Wallet::exarch();
    let custom = config::load_declared(&wallet.declarations, wallet.label)?;
    let disk_warn_bytes = config::disk_warn_bytes()?;
    // Opened once at the trunk; every spawned child inherits this ledger.
    let egress = egress::Egress::open(app::EXARCH)?;

    // SAFETY: startup is still single-threaded — the tokio runtime and the
    // session's workers come later — so nothing races this env mutation.  It is
    // the only scrub, so every child inherits an environment free of keys.
    let mut store = provider::credential::CredentialStore::resolve_and_scrub(custom);
    store.read_vault(&wallet.keychain);
    let available = store.available();
    if available.is_empty() {
        return Err(
            "no provider available: set a provider API key (e.g. ANTHROPIC_API_KEY, \
             OPENAI_API_KEY, OPENROUTER_API_KEY, DEEPSEEK_API_KEY), then store others with /providers"
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
        c.via.as_ref(),
        saved.as_ref(),
        &available,
        |account| provider::models::listing_of(&holdings.catalog, &account.id),
    )?;
    let roles = provider::roles::bindings(
        &c.role,
        &c.role_provider,
        &c.role_via,
        &available,
        |account| provider::models::listing_of(&holdings.catalog, &account.id),
    )?;

    let invocation = policy::for_invocation(&cwd, &c.base, c.extend.as_deref(), &c.restrict)?;
    let scratch =
        Arc::new(app::Scratch::new(app::EXARCH).map_err(|e| format!("scratch dir: {e}"))?);
    let (_mode, terminal, _warn) = ral_core::terminal::TerminalState::probe_from_env();
    boot::face_process_signals(&terminal);
    // Held for the whole run: the lock is the launcher's, not the node's.
    let (run_dir, _run_lock, resumes) = resolve_run(&cwd, c.resume)?;
    if headless && resumes {
        let open = record::AgentLog::live_roots(&run_dir.join("sessions"))
            .map_err(|e| format!("could not inspect {}: {e}", run_dir.display()))?
            .len();
        if open != 1 {
            return Err(format!(
                "this run has {open} open sessions: resume it in the TUI, which reopens all of them"
            ));
        }
    }
    let config_dir = app::EXARCH.xdg_dir(ral_core::path::XdgKind::Config);
    let cwd_path = std::path::PathBuf::from(&cwd);
    // Chat registers no tools, so there is nothing for a system prompt to say.
    let system = if c.chat {
        prompt::CHAT_SYSTEM.to_string()
    } else {
        prompt::assemble(
            &c.system_files,
            &invocation,
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
    let roles = provider::roles::Roles::mint(roles, &bureau, &tuning, c.max_tokens)?;
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
        caps: invocation.stack,
        run_dir: run_dir.clone(),
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
        factory: None,
        bureau: Arc::clone(&bureau),
        roles,
    };
    let seat = agent::RootSeat::Identity {
        scratch,
        cwd: cwd_path,
        terminal,
    };
    let roots: Vec<_> = if resumes {
        Avatar::resume(config, seat, &provider).map(|roots| {
            roots
                .into_iter()
                .map(|(root, resumed)| (root, Some(resumed)))
                .collect()
        })
    } else {
        Avatar::root(config, seat, Arc::clone(&provider)).map(|root| vec![(root, None)])
    }
    .map_err(|e| format!("session init: {e}"))?;

    let info = SessionInfo {
        system_size,
        system_files: &c.system_files,
        base: &c.base,
        extend: c.extend.as_deref(),
        restrict_files: &invocation.restricts,
        cwd: &cwd,
    };
    if headless {
        let [(mut session, _)] =
            <[_; 1]>::try_from(roots).map_err(|_| "a headless run holds exactly one root")?;
        headless::run(&mut session, &info, &provider, seed, c.output_format)
    } else {
        let screen = match screen {
            Some(screen) => screen,
            None => tui::enter(&run_dir)?,
        };
        tui::run(screen, roots, &info, &bureau, seed, c.vi)
    }
}

/// Resolve a fresh or resumable run directory, retaining the lock for the
/// process that owns it; the flag says whether its open sessions are to be
/// resumed.
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
            let open = record::AgentLog::live_roots(&run_dir.join("sessions"))
                .map_err(|error| format!("cannot inspect {}: {error}", run_dir.display()))?;
            if open.is_empty() {
                if explicit {
                    return Err(format!(
                        "--resume target {} has no open session; pass a run directory with a sessions/<n>/record.jsonl that was not closed with /close",
                        run_dir.display()
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
            "--resume found no unlocked run with an open session under {}",
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
/// choice, or a remembered one no longer stands — one chosen on a screen taken
/// in `attended`, the run directory of a launch with a human at it. That
/// screen is handed back for the session to keep.
///
/// # Errors
/// If a named choice does not build, if a choice is needed with no human to
/// make it, or if the user backs out of the picker.
fn open(
    opening: Opening,
    tuning: &provider::Tuning,
    attended: Option<&std::path::Path>,
    bureau: &Bureau,
    max_tokens: Option<u32>,
) -> Result<(Arc<provider::Provider>, Option<tui::TerminalGuard>), String> {
    let build = |choice: Choice| bureau.build(choice, tuning, max_tokens);
    let choose = |among, why: Option<String>| {
        let Some(run_dir) = attended else {
            return Err(format!(
                "{}: pass --model NAME for a headless run",
                why.as_deref().unwrap_or("no model chosen")
            ));
        };
        let mut screen = tui::enter(run_dir)?;
        let pick = tui::choose(&mut screen, bureau, among, tuning, why)?;
        let provider = bureau.build(pick.choice, &pick.tuning, max_tokens)?;
        Ok((provider, Some(screen)))
    };
    match opening {
        Opening::Named(choice) => Ok((build(choice)?, None)),
        Opening::Restored(choice, among) => match build(choice) {
            Ok(provider) => Ok((provider, None)),
            Err(why) => choose(
                among,
                Some(format!("could not restore the saved model: {why}")),
            ),
        },
        Opening::Choose(among, why) => choose(among, why),
    }
}
