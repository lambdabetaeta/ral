//! Exarch — a delegate driving ral in process under a user-chosen grant policy.
//!
//! The whole agent is here: the CLI, capability composition, the
//! [`agent::Agent`] exchange driver, the [`provider::Provider`] transport, and
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
pub mod config;
pub mod egress;
pub mod fleet;
pub mod headless;
pub mod net_policy;
pub mod policy;
pub mod prompt;
pub mod provider;
pub mod shell_eval;
pub(crate) mod sync;
pub mod tui;

use agent::Agent;
use clap::Parser;
use provider::{Engine, Provider};
use std::sync::Arc;
use tui::SessionInfo;

/// Pre-`main` trampoline shared by the binary and every test binary: dress a
/// sandbox-IPC child's fresh shell with exarch's host builtins, then serve
/// any helper re-exec.
///
/// A pipeline stage re-execs the running binary, which under `cargo test` is
/// the libtest harness, so the flag must be served before libtest sees argv
/// and rejects it.
pub fn install_child_hooks_and_serve_helpers() -> Option<u8> {
    ral_core::sandbox::set_child_shell_extension(shell_eval::builtins::host_surface);
    #[cfg(unix)]
    if std::env::args().any(|a| a == "--engine") {
        ral_core::engine::run_engine(&[ral_core::engine::EngineInstaller {
            tag: shell_eval::builtins::INSTALLER_TAG,
            boot: bootstrap::engine_boot_shell,
        }]);
    }
    if let Some(code) = ral_core::try_run_pipeline_stage_helper() {
        return Some(code);
    }
    if let Some(code) = ral_core::test_helper::try_run_test_helper() {
        return Some(code);
    }
    None
}

/// The full pre-`main` dispatch — helper re-execs, then the OS-sandbox stage —
/// shared by the binary's `main` and every test `#[ctor]`.
///
/// `Some(code)` means this process is a re-exec child that should exit now.
pub fn dispatch_pre_main() -> Option<u8> {
    install_child_hooks_and_serve_helpers().or_else(ral_core::sandbox::serve_sandbox_early_init)
}

/// Emit the `#[ctor]` running [`dispatch_pre_main`], which exits a re-exec child
/// before libtest sees flags it would reject.  Once per binary; gate with
/// `#[cfg(test)]` where only the test build wants it.
#[macro_export]
macro_rules! pre_main_ctor {
    () => {
        #[ctor::ctor(unsafe)]
        fn init_pre_main() {
            if let Some(code) = $crate::dispatch_pre_main() {
                ::std::process::exit(i32::from(code));
            }
        }
    };
}

#[cfg(test)]
pre_main_ctor!();

/// The binary's entry point, lifted into the library so integration tests can
/// link the whole crate.
///
/// It parses the CLI, composes the capability lattice, builds an [`Agent`] +
/// [`Provider`], and hands off to a frontend.
///
/// # Errors
/// Returns `Err` if the CLI is misused, if no provider is available, or if
/// loading the provider config, resolving the model selection, building the
/// capability policy, setting up the scratch/log directories, or the chosen
/// frontend fails.
///
/// # Panics
/// Panics if the selected provider is absent from the credential store, an
/// invariant the selection step upholds.
pub fn run() -> Result<(), String> {
    let c = cli::Cli::parse();
    // Subcommands act and exit before the provider-availability check below,
    // since `login` is how the OpenAI provider becomes available.
    if let Some(command) = c.command {
        return match command {
            cli::Command::Login { device_auth } => provider::oauth::login(device_auth),
            cli::Command::Logout { account, all } => provider::oauth::logout(account, all),
            cli::Command::Accounts => {
                let accounts = provider::oauth::load_all();
                if accounts.is_empty() {
                    eprintln!("No ChatGPT accounts signed in. Run `exarch login` to add one.");
                } else {
                    // The label is the login email when there is one, else the
                    // account id itself — which printing twice would be noise.
                    for token in accounts {
                        if token.email.is_some() {
                            println!("{} ({})", token.label(), token.account_id);
                        } else {
                            println!("{}", token.account_id);
                        }
                    }
                }
                Ok(())
            }
        };
    }
    // Doubles the `requires = "headless"` in `cli`: `--output-format` reaches
    // only the headless frontend, so asking for `json` alone is a mistake.
    if c.output_format == headless::OutputFormat::Json && !c.headless {
        return Err("--output-format is only meaningful with --headless".into());
    }
    let seed = cli::load_seed(c.prompt, c.file, c.trailing_prompt)?;

    let custom = config::load()?;
    let disk_warn_bytes = config::disk_warn_bytes()?;
    // Opened once at the trunk; every spawned child inherits this ledger.
    let egress = egress::Egress::open(bootstrap::EXARCH)?;

    // SAFETY: startup is still single-threaded — the tokio runtime and the
    // session's workers come later — so nothing races this env mutation.  It is
    // the only scrub, so every child inherits an environment free of keys.
    let mut store = provider::credential::CredentialStore::resolve_and_scrub(custom);
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

    let mut catalog = provider::models::ModelCatalog::new(
        provider::models::LiveSource::new(&store),
        bootstrap::EXARCH,
    );
    let (id, model, mut tuning, route) = resolve_initial_selection(
        c.provider.as_deref(),
        c.model.as_deref(),
        &state_dir,
        &available,
        &mut catalog,
    )?;
    if let Some(keyword) = c.effort.as_deref() {
        tuning.effort = Some(provider::ReasoningEffort::from_keyword(keyword).ok_or_else(
            || {
                format!(
                    "invalid effort '{keyword}' — expected zero|low|medium|high|xhigh|max|minimal"
                )
            },
        )?);
    }
    // An unset model means "the `/model` picker will choose"; a headless run has
    // no picker, so it must be told one.
    if model.is_empty() && c.headless {
        return Err(format!(
            "custom provider '{}' has no default model — pass --model NAME for a headless run",
            id.label()
        ));
    }
    // A `--model` override is a deliberate choice, remembered like the picker's,
    // so the next launch in this project restores it.
    if c.model.is_some() && !model.is_empty() {
        let _ = provider::state::save(
            &state_dir,
            &provider::state::State::new(&id, &model, &tuning, route.as_deref()),
        );
    }
    let label = id.label();
    let cred = store
        .get(&id)
        .expect("selected provider must be available")
        .clone();

    let (caps, restrict_files) =
        policy::for_invocation(&cwd, &c.base, c.extend_base.as_deref(), &c.restrict)?;
    // The persisted selection lives under the XDG state home, outside the
    // agent's cwd sandbox, so no deny-list entry is needed to keep tools off it.

    // A throwaway shell carrying the same grants, only so
    // `RAL_DUMP_SANDBOX_PROFILE` can print the profile an external child would
    // be sandboxed under.
    {
        let mut probe = ral_core::Shell::new(ral_core::io::TerminalState::default());
        probe.push_session_capabilities(caps.clone());
        if let Some(projection) = probe.sandbox_projection() {
            ral_core::sandbox::dump_profile_if_requested(&projection);
        }
    }
    let scratch = Arc::new(
        bootstrap::Scratch::new(bootstrap::EXARCH).map_err(|e| format!("scratch dir: {e}"))?,
    );
    let (run_dir, run_lock, resume) = resolve_run(&cwd, c.resume, c.no_logs)?;
    let config_dir = bootstrap::EXARCH.xdg_dir(ral_core::path::basedir::XdgKind::Config);
    let cwd_path = std::path::PathBuf::from(&cwd);
    // Whether the double fork exists on this host at all; whether a given call
    // may spend it is asked of the live grant stack (`detach:`).  A sandboxed
    // session keeps the verb — a survivor carries its projection for life.
    let detach = cfg!(unix);
    // Chat registers no tools, so there is nothing for a system prompt to say.
    let system = if c.chat {
        prompt::CHAT_SYSTEM.to_string()
    } else {
        prompt::assemble(
            &c.system_files,
            &caps,
            &scratch,
            &cwd_path,
            &config_dir,
            !c.headless,
            c.edit,
        )?
    };
    let system_size = system.len();

    // One runtime for the whole fleet; per-credential transports warm lazily.
    let engine = Engine::new();
    let provider = Arc::new(Provider::build(
        engine.clone(),
        &id,
        model.clone(),
        &cred,
        c.max_tokens,
        tuning,
        route,
    ));
    let mut session = Agent::root(
        agent::RootConfig {
            system,
            caps,
            run_dir: run_dir.clone(),
            resume,
            no_logs: c.no_logs,
            run_lock,
            model,
            provider_label: label.to_string(),
            allow_schedule: c.allow_schedule,
            // An interactive trunk parks for the human; a headless one
            // terminates once its seeded work is idle.
            interactive: !c.headless,
            chat: c.chat,
            disk_warn_bytes,
            fuel: agent::SPAWN_FUEL,
            egress,
            hatchery: None,
        },
        agent::RootSeat::Identity {
            scratch: Arc::clone(&scratch),
            cwd: cwd_path,
            detach,
        },
        Arc::clone(&provider),
    )
    .map_err(|e| format!("session init: {e}"))?;

    let info = SessionInfo {
        system_size,
        system_files: &c.system_files,
        base: &c.base,
        extend_base: c.extend_base.as_deref(),
        restrict_files: &restrict_files,
        scratch: scratch.path(),
        cwd: &cwd,
    };
    if c.headless {
        headless::run(
            &mut session,
            &info,
            &provider,
            seed,
            c.output_format,
            Arc::clone(&engine),
        )
    } else {
        tui::run(
            &mut session,
            &provider,
            &info,
            &mut store,
            &mut catalog,
            &run_dir,
            seed,
            c.vi,
            Arc::clone(&engine),
        )
    }
}

/// Resolve a fresh or resumable run directory, retaining the lock for the
/// process that owns it.
#[allow(
    clippy::option_option,
    reason = "the CLI distinguishes absent, bare, and named resume"
)]
fn resolve_run(
    cwd: &str,
    resume: Option<Option<std::path::PathBuf>>,
    no_logs: bool,
) -> Result<
    (
        std::path::PathBuf,
        Option<bootstrap::RunLock>,
        Option<std::path::PathBuf>,
    ),
    String,
> {
    if let Some(target) = resume {
        let explicit = target.is_some();
        let candidates = match target {
            Some(target) => vec![bootstrap::normalize_resume_target(&target)?],
            None => bootstrap::EXARCH
                .resume_candidates(cwd)
                .map_err(|error| format!("could not inspect resumable runs: {error}"))?,
        };
        for run_dir in candidates {
            let events = run_dir.join("sessions/0/events.jsonl");
            if !events.is_file() {
                if explicit {
                    return Err(format!(
                        "--resume target {} has no {}; pass a run directory containing sessions/0/events.jsonl",
                        run_dir.display(),
                        events.display()
                    ));
                }
                continue;
            }
            match bootstrap::RunLock::try_acquire(&run_dir) {
                Ok(lock) => return Ok((run_dir.clone(), Some(lock), Some(run_dir))),
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
            "--resume found no unlocked run with sessions/0/events.jsonl under {}",
            bootstrap::EXARCH.project_dir(cwd).display()
        ));
    }

    let run_dir = bootstrap::EXARCH
        .log_run_dir(cwd)
        .map_err(|error| format!("log dir: {error}"))?;
    let lock = (!no_logs)
        .then(|| bootstrap::RunLock::try_acquire(&run_dir))
        .transpose()
        .map_err(|error| format!("could not lock {}: {error}", run_dir.display()))?;
    Ok((run_dir, lock, None))
}

fn resolve_initial_selection(
    provider_override: Option<&str>,
    model_override: Option<&str>,
    state_dir: &std::path::Path,
    available: &[provider::ProviderId],
    catalog: &mut provider::models::ModelCatalog<provider::models::LiveSource>,
) -> Result<
    (
        provider::ProviderId,
        String,
        provider::Tuning,
        Option<String>,
    ),
    String,
> {
    if let Some(pname) = provider_override {
        let id = provider::models::resolve_pinned_provider(pname, available)?;
        let model = match model_override {
            Some(m) => m.to_string(),
            None => match id.famous() {
                Some(kind) => kind.info().1.to_string(),
                None => {
                    return Err(format!(
                        "provider '{pname}' has no default model — also pass --model NAME",
                    ));
                }
            },
        };
        return Ok((id, model, provider::Tuning::initial(), None));
    }
    if let Some(name) = model_override {
        let id = provider::models::resolve_model_provider(name, available, catalog)?;
        return Ok((id, name.to_string(), provider::Tuning::initial(), None));
    }
    if let Some(saved) = provider::state::load(state_dir)
        && let Some(id) = saved.provider_id(available)
    {
        let tuning = saved.tuning();
        return Ok((id, saved.model, tuning, saved.route));
    }
    let id = available[0].clone();
    match id.famous() {
        Some(kind) => Ok((
            id,
            kind.info().1.to_string(),
            provider::Tuning::initial(),
            None,
        )),
        // No built-in default is no reason to refuse to launch: open with the
        // model unset — the empty sentinel — so the interactive frontend lands
        // on its `/model` hint.  `run` rejects that for a headless launch.
        None => Ok((id, String::new(), provider::Tuning::initial(), None)),
    }
}
