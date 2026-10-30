//! A coding agent based on the ral shell, driven in process under a
//! user-chosen grant policy.
//!
//! The whole agent is here: the CLI, capability composition, the
//! [`agent::Agent`] exchange driver, the [`provider::Provider`] transport, and
//! the two frontends ([`tui::run`] / [`headless::run`]).  The `exarch` binary is
//! a thin shell over [`run`]; integration tests link this library directly.
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_macros,
        reason = "[test] libtest captures print macros"
    )
)]
pub(crate) mod agent;
pub mod app;
pub(crate) mod boot;
pub(crate) mod bus;
pub mod card;
pub(crate) mod cli;
pub mod clock;
pub mod config;
pub(crate) mod enquiry;
pub(crate) mod headless;
pub mod host;
pub(crate) mod latch;
pub(crate) mod library;
pub mod net_policy;
pub mod policy;
pub mod port;
pub mod prompt;
pub mod provider;
pub mod record;
pub(crate) mod schedule;
pub(crate) mod shell_eval;
pub(crate) mod signals;
mod site;
pub(crate) mod skill;
#[cfg(all(unix, test))]
#[allow(clippy::disallowed_methods, reason = "test code")]
pub(crate) mod test_helper;
pub(crate) mod tui;
pub mod wallet;

use crate::site::{Silent, site};
use clap::Parser;
use host::Host;
use provider::state::{Opening, opening};
use provider::{Bureau, Choice, Holdings, Pool};
use ral_core::own::{self, OwnDir};
use ral_core::path::{Kind, XdgKind};
use std::sync::Arc;

/// The one boot recipe table, for the identity engine and the `--engine` child
/// alike.
pub static INSTALLERS: [ral_core::engine::EngineInstaller; 1] =
    [ral_core::engine::EngineInstaller {
        tag: shell_eval::builtins::INSTALLER_TAG,
        runtime: boot::engine_runtime,
        root: boot::engine_root,
        narrow: policy::base_layer,
    }];

/// The full pre-`main` dispatch for a `main`: core's
/// [`serve_process`](ral_core::invocation::serve_process) over exarch's
/// [`INSTALLERS`], and the exit its answer calls for
/// ([`exit_if_served`](ral_core::invocation::exit_if_served)).
///
/// Returns only when this process is no re-exec child, so a `main` calls it as
/// its first real statement.  A `#[ctor]` calls
/// [`exit_if_re_exec_child_before_main`] instead, which differs only in where
/// it reads argv.
pub fn exit_if_re_exec_child() {
    ral_core::invocation::exit_if_served(ral_core::invocation::serve_process(&INSTALLERS));
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
    ral_core::invocation::exit_if_served(ral_core::invocation::serve_process_before_main(
        &INSTALLERS,
    ));
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
    pub cwd: &'a ral_core::path::CwdPath,
}

/// The binary's entry point, lifted into the library so integration tests can
/// link the whole crate.
///
/// It parses the CLI, composes the capability lattice, builds a [`Host`] and
/// [`provider::Provider`], and hands off to a frontend.
///
/// # Errors
/// Returns `Err` if the CLI is misused, if no provider is available, or if
/// loading the provider config, resolving the model selection, building the
/// capability policy, setting up the scratch/log directories, or the chosen
/// frontend fails.
pub fn run() -> Result<(), String> {
    let mut c = cli::Cli::parse();
    let headless = c.is_headless();
    // Subcommands act and exit before the provider-availability check below,
    // since `login` is how the OpenAI provider becomes available.
    if let Some(command) = c.command.take() {
        return match command {
            cli::Command::Login {
                service,
                account,
                device_auth,
            } => provider::oauth::login(service, account, device_auth),
            cli::Command::Logout { account, all } => provider::oauth::logout(account, all),
            cli::Command::Accounts => {
                let accounts = provider::oauth::accounts();
                if accounts.is_empty() {
                    ral_core::errln!(
                        "No accounts signed in. Run exarch login <service> to add one (openai-chatgpt, openai-codex, xai-grok)."
                    );
                } else {
                    for account in &accounts {
                        ral_core::outln!("{}", provider::identity::label(account, &accounts));
                    }
                }
                Ok(())
            }
            cli::Command::Export { session, out } => {
                // The session directory the user names, or the record itself.
                let record = match site!(Silent::CliArgument, own::kind_named(&session)) {
                    Some(Kind::Dir) => session.join(record::RECORD),
                    _ => session,
                };
                let file = site!(Silent::CliArgument, own::open_named(&record)).map_err(|e| {
                    format!("{e}; is this a session directory under $XDG_STATE_HOME/exarch?")
                })?;
                match &out {
                    Some(dest) => tui::export_to(file, &record, dest, tui::Format::Plain),
                    None => tui::export(
                        file,
                        &record,
                        &mut std::io::stdout().lock(),
                        tui::Format::Plain,
                    ),
                }
                .map_err(|e| format!("could not export {}: {e}", ral_core::path::shown(&record)))
            }
        };
    }
    let seed = cli::load_seed(c.prompt.take(), c.file.take())?;

    let store = wallet::Wallet::of(app::EXARCH, config::CONFIG_FILE, config::LABEL)?.prepare()?;
    let disk_warn_bytes = config::disk_warn_bytes()?;
    let search = net_policy::load()?.search;

    let available = store.available();
    if available.is_empty() {
        return Err(
            "no provider available: set a provider API key (e.g. ANTHROPIC_API_KEY, \
             OPENAI_API_KEY, OPENROUTER_API_KEY, DEEPSEEK_API_KEY), then store others with /providers"
                .into(),
        );
    }

    let launch_cwd = ral_core::host::cwd().ok_or("launch cwd: unavailable")?;
    let state_dir = app::EXARCH.project(launch_cwd.as_path())?;

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

    // Before the policy freezes: a `scratch` place in a profile anchors to it.
    let scratch =
        Arc::new(app::Scratch::new(app::EXARCH).map_err(|e| format!("scratch dir: {e}"))?);
    let invocation = policy::for_invocation(
        &launch_cwd,
        scratch.spelling(),
        &c.base,
        c.extend.as_deref(),
        &c.restrict,
        ral_core::authority::sandbox::pinned_arg0(),
    )?;
    let (_mode, terminal, _warn) = ral_core::io::terminal::TerminalState::probe_from_env();
    boot::face_process_signals(&terminal);
    // Held for the whole run: the lock is the launcher's, not the node's.
    let (run, _run_lock, resumes) = resolve_run(launch_cwd.as_path(), c.resume.take())?;
    if headless && resumes {
        let open = record::AgentLog::human_children(&run.dir("sessions")?)
            .map_err(|e| format!("could not inspect {run}: {e}"))?
            .len();
        if open != 1 {
            return Err(format!(
                "a headless run reopens exactly one session; this run has {open} open: resume it in the TUI, which reopens all of them"
            ));
        }
    }
    let config_dir = app::EXARCH.xdg_dir(XdgKind::Config);
    // The boot's own reads are judged under the baseline of a runtime built here,
    // before any is made; a host that cannot resolve its baseline boots nothing.
    let boot_floor =
        ral_core::Runtime::bare(terminal).map_err(|e| format!("exarch cannot start: {e}"))?;
    // Chat registers no tools, so there is nothing for a system prompt to say.
    let system = if c.chat {
        prompt::CHAT_SYSTEM.to_string()
    } else {
        prompt::assemble(
            &c.system_files,
            &invocation,
            boot_floor.baseline(),
            app::EXARCH,
            &launch_cwd,
            config_dir.as_deref(),
            !headless,
            c.edit,
        )?
    };
    let system_size = system.len();

    // One runtime for the whole fleet; per-credential transports warm lazily.
    let engine = Pool::new();
    let bureau = Arc::new(Bureau::Live { engine, holdings });
    let roles = provider::roles::Roles::mint(roles, &bureau, &tuning, c.max_tokens)?;
    let (provider, screen) = open(
        opening,
        &tuning,
        (!headless).then_some(&run),
        &bureau,
        c.max_tokens,
    )?;
    // The selection a launch opens with is the one the next launch restores.
    let _ = provider::state::save(
        &state_dir,
        &provider::state::State::of(&provider, &available),
    );
    let config = launch_config(
        &c,
        system,
        run.clone(),
        disk_warn_bytes,
        search,
        &bureau,
        roles,
    );
    let engine = agent::RootEngine::Identity {
        scratch,
        cwd: launch_cwd.clone(),
        grants: invocation.stack,
        terminal,
    };
    let host = if resumes {
        Host::resume(config, engine, &provider)
    } else {
        Host::found(config, engine, Arc::clone(&provider))
    }
    .map_err(|e| format!("session init: {e}"))?;

    let info = SessionInfo {
        system_size,
        system_files: &c.system_files,
        base: &c.base,
        extend: c.extend.as_deref(),
        restrict_files: &invocation.restricts,
        cwd: &launch_cwd,
    };
    let port = host.port();
    let result = if headless {
        if host.children().len() == 1 {
            headless::run(&host, &info, &provider, seed, c.output_format)
        } else {
            Err("a headless run holds exactly one session".into())
        }
        .map(|()| None)
    } else {
        let screen = match screen {
            Some(screen) => Ok(screen),
            None => tui::enter(&run),
        };
        screen
            .and_then(|screen| {
                tui::run(
                    screen,
                    port.clone(),
                    host.feed(),
                    &info,
                    &bureau,
                    seed,
                    c.vi,
                )
            })
            .map(Some)
    };
    let failures = host.end();
    if let Ok(Some(leaving)) = &result {
        leaving.report(port.usage());
    }
    combine_failures(result.map(drop), &failures)
}

/// The launch's configuration under the parsed command line.
fn launch_config(
    c: &cli::Cli,
    system: String,
    run_dir: OwnDir,
    disk_warn_bytes: Option<u64>,
    search: bool,
    bureau: &Arc<Bureau>,
    roles: provider::roles::Roles,
) -> agent::LaunchConfig {
    let headless = c.is_headless();
    agent::LaunchConfig {
        system,
        run_dir,
        returns: headless,
        admission: agent::Admission::Free,
        helper_idle: c.helper_idle,
        max_tokens: c.max_tokens,
        tools: if c.chat {
            provider::Toolset::default()
        } else {
            provider::Toolset::offered(c.thinking_tool)
        },
        allow_schedule: c.allow_schedule,
        resume_on_reset: !headless,
        disk_warn_bytes,
        fuel: agent::SPAWN_FUEL,
        search,
        factory: None,
        bureau: Arc::clone(bureau),
        roles,
    }
}

fn combine_failures(result: Result<(), String>, failures: &[String]) -> Result<(), String> {
    match (result, failures.is_empty()) {
        (result, true) => result,
        (Ok(()), false) => Err(failures.join("\n")),
        (Err(error), false) => Err(format!("{error}\n{}", failures.join("\n"))),
    }
}

/// Resolve a fresh or resumable run directory, retaining the lock for the
/// process that owns it; the flag says whether its open sessions are to be
/// resumed.
///
/// A named run is reopened from the held state directory ([`OwnDir::within`]),
/// so only a run exarch keeps is accepted.
#[allow(
    clippy::option_option,
    reason = "the CLI distinguishes absent, bare, and named resume"
)]
fn resolve_run(
    cwd: &std::path::Path,
    resume: Option<Option<std::path::PathBuf>>,
) -> Result<(OwnDir, app::RunLock, bool), String> {
    if let Some(target) = resume {
        if let Some(target) = target {
            let run = app::EXARCH
                .own(XdgKind::State)?
                .within(&app::normalize_resume_target(&target)?)?;
            return match lock_resumable(&run)? {
                Ok(lock) => Ok((run, lock, true)),
                Err(Declined::NoOpenSession) => Err(format!(
                    "--resume target {run} has no open session; pass a run directory with a sessions/<n>/{} that was not closed with /close",
                    record::RECORD
                )),
                Err(Declined::InUse) => Err(format!(
                    "--resume target {run} is already running; its lock is held by another exarch"
                )),
            };
        }
        let project = app::EXARCH.project(cwd)?;
        let candidates = app::resume_candidates(&project)
            .map_err(|error| format!("could not inspect resumable runs: {error}"))?;
        for name in candidates {
            let run = project.dir(&name)?;
            if let Ok(lock) = lock_resumable(&run)? {
                return Ok((run, lock, true));
            }
        }
        return Err(format!(
            "--resume found no unlocked run with an open session under {project}"
        ));
    }

    let run = app::EXARCH
        .new_run(cwd)
        .map_err(|error| format!("log dir: {error}"))?;
    let lock = app::RunLock::try_acquire(&run)
        .map_err(|error| format!("could not lock {run}: {error}"))?;
    Ok((run, lock, false))
}

/// Why a run is not to be resumed.
enum Declined {
    /// Every session of it was closed, or none recorded.
    NoOpenSession,
    /// Another exarch holds its lock.
    InUse,
}

/// `run`'s lock, taken to resume it: the lock, or why the run is declined.
///
/// # Errors
/// A run that cannot be inspected or locked.
fn lock_resumable(run: &OwnDir) -> Result<Result<app::RunLock, Declined>, String> {
    let open = run
        .dir("sessions")
        .map_err(std::io::Error::from)
        .and_then(|sessions| record::AgentLog::human_children(&sessions))
        .map_err(|error| format!("cannot inspect {run}: {error}"))?;
    if open.is_empty() {
        return Ok(Err(Declined::NoOpenSession));
    }
    match app::RunLock::try_acquire(run) {
        Ok(lock) => Ok(Ok(lock)),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(Err(Declined::InUse)),
        Err(error) => Err(format!("cannot resume {run}: {error}")),
    }
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
    attended: Option<&OwnDir>,
    bureau: &Bureau,
    max_tokens: Option<u32>,
) -> Result<(Arc<provider::Provider>, Option<tui::TerminalGuard>), String> {
    let build = |choice: Choice| bureau.build(choice, tuning, max_tokens);
    let choose = |among, why: Option<String>| {
        let Some(run) = attended else {
            return Err(format!(
                "{}: pass --model NAME for a headless run",
                why.as_deref().unwrap_or("no model chosen")
            ));
        };
        let mut screen = tui::enter(run)?;
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

#[cfg(test)]
mod tests {
    use super::{Host, combine_failures, launch_config};
    use crate::agent::RootEngine;
    use crate::app::{self, Scratch};
    use crate::provider::roles::Roles;
    use crate::provider::scripted::Script;
    use crate::provider::{Bureau, Provider};
    use clap::Parser;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn combining_failures_keeps_the_frontend_verdict_then_teardown_lines() {
        let teardown = ["a".to_string(), "b".to_string()];
        assert_eq!(combine_failures(Ok(()), &[]), Ok(()));
        assert_eq!(
            combine_failures(Err("frontend".into()), &[]),
            Err("frontend".into())
        );
        assert_eq!(combine_failures(Ok(()), &teardown), Err("a\nb".into()));
        assert_eq!(
            combine_failures(Err("frontend".into()), &teardown),
            Err("frontend\na\nb".into())
        );
    }

    #[test]
    fn the_parsed_helper_idle_reaches_the_founded_fleet() {
        let dir = ral_core::own::OwnDir::for_test("launch-config").unwrap();
        let found = |args: &[&str]| {
            let c = crate::cli::Cli::try_parse_from(
                std::iter::once("exarch").chain(args.iter().copied()),
            )
            .unwrap();
            let cfg = launch_config(
                &c,
                "system".into(),
                (*dir).clone(),
                None,
                false,
                &Arc::new(Bureau::scripted([])),
                Roles::default(),
            );
            let engine = RootEngine::Identity {
                scratch: Arc::new(Scratch::for_test(app::EXARCH, "launch-config").unwrap()),
                cwd: ral_core::host::cwd().unwrap(),
                grants: ral_core::authority::policy::GrantStack::default(),
                terminal: ral_core::io::terminal::TerminalState::default(),
            };
            let provider = Arc::new(Provider::scripted("test-model", Script::new()));
            Host::found(cfg, engine, provider).unwrap()
        };
        for (args, idle) in [
            (&[][..], crate::agent::fleet::HELPER_IDLE),
            (&["--helper-idle", "90m"][..], Duration::from_mins(90)),
        ] {
            let host = found(args);
            assert_eq!(host.fleet().launch.idle, idle);
            assert_eq!(host.end(), [] as [String; 0]);
        }
    }
}
