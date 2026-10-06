//! A coding agent based on the ral shell, driven in process under a
//! user-chosen grant policy.
//!
//! The whole agent is here: the CLI, capability composition, the
//! [`agent::Avatar`] exchange driver, the [`provider::Provider`] transport, and
//! the two frontends ([`tui::run`] / [`headless::run`]).  The `exarch` binary is
//! a thin shell over [`run`]; integration tests link this library directly.
#![allow(
    clippy::disallowed_methods,
    reason = "exarch is an application, not the ral shell; the clippy.toml invariants target ral-core's Shell path/cwd/fs discipline"
)]
pub mod agent;
pub mod bootstrap;
pub mod bus;
pub mod cli;
pub mod clock;
pub mod config;
pub mod egress;
pub mod fleet;
pub mod headless;
pub(crate) mod latch;
pub mod net_policy;
pub mod policy;
pub mod prompt;
pub mod provider;
pub mod record;
pub mod shell_eval;
pub(crate) mod signals;
pub mod tui;

use agent::Avatar;
use clap::Parser;
use provider::{Bureau, Engine, Holdings};
use std::sync::Arc;
use tui::SessionInfo;

/// The one boot recipe table, for the identity seat and the `--engine` child
/// alike.
pub static INSTALLERS: [ral_core::engine::EngineInstaller; 1] =
    [ral_core::engine::EngineInstaller {
        tag: shell_eval::builtins::INSTALLER_TAG,
        boot: bootstrap::engine_boot_shell,
        narrow: policy::base_layer,
    }];

/// The full pre-`main` dispatch, shared by the binary's `main` and every test
/// `#[ctor]`: core's [`serve_pre_main`](ral_core::sandbox::serve_pre_main) over
/// exarch's [`INSTALLERS`].
///
/// The pipeline anchor re-execs the running binary, which under `cargo test`
/// is the libtest harness, so the flag must be served before libtest sees argv
/// and rejects it.
///
/// `Some(code)` means this process is a re-exec child that should exit now.
pub fn dispatch_pre_main() -> Option<u8> {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    ral_core::sandbox::serve_pre_main(&ral_core::classify(&argv), &INSTALLERS)
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
            reason = "a re-exec child, served before any CLI: the anchor drains a pipe, the pgid probe writes a line, the sandbox stage has already exec'd or refused. None boots a shell, so none holds a lease, a watched child or a staged write — and in the `#[ctor]` form nothing at all is booted yet"
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
            cli::Command::Login { device_auth } => provider::oauth::login(device_auth),
            cli::Command::Logout { account, all } => provider::oauth::logout(account, all),
            cli::Command::Accounts => {
                let accounts = provider::oauth::accounts();
                if accounts.is_empty() {
                    eprintln!("No ChatGPT accounts signed in. Run `exarch login` to add one.");
                } else {
                    for account in &accounts {
                        println!("{}", provider::identity::label(account, &accounts));
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
    let egress = egress::Egress::open(bootstrap::EXARCH)?;

    // SAFETY: startup is still single-threaded — the tokio runtime and the
    // session's workers come later — so nothing races this env mutation.  It is
    // the only scrub, so every child inherits an environment free of keys.
    let store = provider::credential::CredentialStore::resolve_and_scrub(custom);
    let available = store.available();
    if available.is_empty() {
        return Err(
            "no provider available — set a provider API key (e.g. ANTHROPIC_API_KEY, \
             OPENAI_API_KEY, OPENROUTER_API_KEY, DEEPSEEK_API_KEY)"
                .into(),
        );
    }

    let cwd = std::env::current_dir()
        .map_err(|e| format!("launch cwd: {e}"))?
        .to_string_lossy()
        .into_owned();
    let state_dir = bootstrap::EXARCH.project_dir(&cwd);

    let holdings = Holdings::new(store, bootstrap::EXARCH);
    // The saved selection is the ground each flag is laid over, so pinning a
    // model keeps the effort rung the picker last chose rather than resetting it.
    let saved = provider::state::load(&state_dir);
    let mut tuning = saved
        .as_ref()
        .map_or_else(provider::Tuning::initial, provider::state::State::tuning);
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
    // A throwaway shell carrying the same grants, only so
    // `RAL_DUMP_SANDBOX_PROFILE` can print the profile an external child would
    // be sandboxed under.
    {
        let mut probe = ral_core::Shell::new(ral_core::io::TerminalState::default());
        for layer in &caps {
            probe.push_session_capabilities(layer.clone());
        }
        if let Some(projection) = probe.sandbox_projection() {
            ral_core::sandbox::dump_profile(&projection);
        }
    }
    let scratch = Arc::new(
        bootstrap::Scratch::new(bootstrap::EXARCH).map_err(|e| format!("scratch dir: {e}"))?,
    );
    let (_mode, terminal, _warn) = ral_core::io::TerminalState::probe_from_env();
    bootstrap::face_process_signals(&terminal);
    // Held for the whole run: the lock is the launcher's, not the node's.
    let (run_dir, _run_lock, resumes) = resolve_run(&cwd, c.resume)?;
    let config_dir = bootstrap::EXARCH.xdg_dir(ral_core::path::basedir::XdgKind::Config);
    let cwd_path = std::path::PathBuf::from(&cwd);
    // Chat registers no tools, so there is nothing for a system prompt to say.
    let system = if c.chat {
        prompt::CHAT_SYSTEM.to_string()
    } else {
        prompt::assemble(
            &c.system_files,
            &caps,
            bootstrap::EXARCH,
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
        account: agent::RecordedAccount::of(provider.account(), &available),
        // An attended trunk parks for the human; a headless one terminates
        // once its seeded work is idle.
        trunk: if headless {
            agent::Trunk::Headless
        } else {
            agent::Trunk::Attended
        },
        tools: if c.chat {
            shell_eval::tools::Toolset::default()
        } else {
            shell_eval::tools::Toolset::offered(c.thinking_tool)
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
) -> Result<(std::path::PathBuf, bootstrap::RunLock, bool), String> {
    if let Some(target) = resume {
        let explicit = target.is_some();
        let candidates = match target {
            Some(target) => vec![bootstrap::normalize_resume_target(&target)?],
            None => bootstrap::EXARCH
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
            match bootstrap::RunLock::try_acquire(&run_dir) {
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
            bootstrap::EXARCH.project_dir(cwd).display()
        ));
    }

    let run_dir = bootstrap::EXARCH
        .log_run_dir(cwd)
        .map_err(|error| format!("log dir: {error}"))?;
    let lock = bootstrap::RunLock::try_acquire(&run_dir)
        .map_err(|error| format!("could not lock {}: {error}", run_dir.display()))?;
    Ok((run_dir, lock, false))
}

/// An account, one of its models, and the `OpenRouter` route chosen for that
/// model.
struct Pair {
    account: provider::Account,
    model: String,
    route: Option<String>,
}

/// What a launch opens on, before the bureau weighs it.
enum Opening {
    /// The pair the flags named; its refusal ends the launch.
    Named(Pair),
    /// The pair remembered for this project; its refusal opens the picker over
    /// these accounts instead.
    Restored(Pair, Vec<provider::Account>),
    /// No pair to try: the picker over these accounts, saying why when that
    /// is news.
    Choose(Vec<provider::Account>, Option<String>),
}

/// What the flags and `saved`, the selection remembered for this project, ask
/// a launch to open on.
///
/// `--model` names a pair outright, on `--provider`'s account or else the one
/// whose listing names the model. `--provider` alone restores the model saved
/// for that account, else asks for one. With neither, the saved pair is
/// restored, else the user is asked. A saved route rides only with the pair it
/// was chosen for.
fn opening(
    provider_flag: Option<&str>,
    model_flag: Option<&str>,
    saved: Option<&provider::state::State>,
    available: &[provider::Account],
    listing: impl FnMut(&provider::Account) -> Result<Vec<provider::models::Listed>, String>,
) -> Result<Opening, String> {
    let pair = |account: provider::Account, model: String| {
        let route = saved
            .filter(|s| s.provider == account.id.as_str() && s.model == model)
            .and_then(|s| s.route.clone());
        Pair {
            account,
            model,
            route,
        }
    };
    let pinned = provider_flag
        .map(|name| provider::models::resolve_pinned_provider(name, available))
        .transpose()?;
    Ok(match (pinned, model_flag) {
        (Some(account), Some(model)) => Opening::Named(pair(account, model.to_string())),
        (None, Some(model)) => Opening::Named(pair(
            provider::models::resolve_model_provider(model, available, listing)?,
            model.to_string(),
        )),
        (Some(account), None) => match saved.filter(|s| s.provider == account.id.as_str()) {
            Some(s) => Opening::Restored(pair(account.clone(), s.model.clone()), vec![account]),
            None => Opening::Choose(vec![account], None),
        },
        (None, None) => match saved {
            None => Opening::Choose(available.to_vec(), None),
            Some(s) => match s.account(available) {
                Some(account) => {
                    Opening::Restored(pair(account, s.model.clone()), available.to_vec())
                }
                None => Opening::Choose(
                    available.to_vec(),
                    Some(format!(
                        "could not restore the saved model: '{}' is no longer available",
                        s.provider_name
                    )),
                ),
            },
        },
    })
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
    let build =
        |pair: Pair| bureau.build(&pair.account, pair.model, tuning, pair.route, max_tokens);
    let choose = |among, why: Option<String>| {
        let Some(run_dir) = attended else {
            return Err(format!(
                "{} — pass --model NAME for a headless run",
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

#[cfg(test)]
mod tests {
    use super::*;
    use provider::Account;
    use provider::models::Listed;
    use provider::state::State;

    fn saved(account: &Account, model: &str, route: Option<&str>) -> State {
        State::new(
            account,
            std::slice::from_ref(account),
            model,
            &provider::Tuning::default(),
            route,
        )
    }

    fn lists(models: &[&str]) -> impl FnMut(&Account) -> Result<Vec<Listed>, String> {
        let listed: Vec<Listed> = models.iter().map(|m| Listed::bare(*m)).collect();
        move |_| Ok(listed.clone())
    }

    #[test]
    fn a_saved_route_rides_only_with_its_own_pair() {
        let openrouter = Account::built_in("openrouter");
        let state = saved(&openrouter, "vendor/model-a", Some("deepinfra"));
        let available = [openrouter];
        let route_for = |model| match opening(
            None,
            Some(model),
            Some(&state),
            &available,
            lists(&["vendor/model-a", "vendor/model-b"]),
        ) {
            Ok(Opening::Named(pair)) => pair.route,
            _ => panic!("--model names a pair"),
        };
        assert_eq!(route_for("vendor/model-a").as_deref(), Some("deepinfra"));
        assert_eq!(route_for("vendor/model-b"), None);
    }

    #[test]
    fn provider_alone_restores_that_accounts_saved_model() {
        let anthropic = Account::built_in("anthropic");
        let state = saved(&anthropic, "model-a", None);
        let available = [anthropic.clone(), Account::built_in("deepseek")];
        match opening(
            Some("anthropic"),
            None,
            Some(&state),
            &available,
            lists(&[]),
        ) {
            Ok(Opening::Restored(pair, among)) => {
                assert_eq!(
                    (pair.account, pair.model.as_str()),
                    (anthropic.clone(), "model-a")
                );
                assert_eq!(among, [anthropic]);
            }
            _ => panic!("the saved model is restored"),
        }
    }

    #[test]
    fn provider_alone_with_nothing_saved_for_it_asks_among_its_models() {
        let anthropic = Account::built_in("anthropic");
        let deepseek = Account::built_in("deepseek");
        let state = saved(&deepseek, "model-a", None);
        let available = [anthropic.clone(), deepseek];
        match opening(
            Some("anthropic"),
            None,
            Some(&state),
            &available,
            lists(&[]),
        ) {
            Ok(Opening::Choose(among, None)) => assert_eq!(among, [anthropic]),
            _ => panic!("a choice among the named account's models"),
        }
    }

    #[test]
    fn a_vanished_saved_account_asks_and_says_why() {
        let gone = Account::declared("gone");
        let state = saved(&gone, "model-a", None);
        let available = [Account::built_in("anthropic")];
        match opening(None, None, Some(&state), &available, lists(&[])) {
            Ok(Opening::Choose(among, Some(why))) => {
                assert_eq!(among, available);
                assert!(why.contains("'gone'"), "got: {why}");
            }
            _ => panic!("a choice, saying why"),
        }
    }

    #[test]
    fn nothing_saved_and_no_flags_asks_among_every_account() {
        let available = [
            Account::built_in("anthropic"),
            Account::built_in("deepseek"),
        ];
        match opening(None, None, None, &available, lists(&[])) {
            Ok(Opening::Choose(among, None)) => assert_eq!(among, available),
            _ => panic!("a choice among every account"),
        }
    }
}
