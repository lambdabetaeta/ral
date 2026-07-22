//! Exarch — a delegate driving ral in process under a user-chosen grant
//! policy.  This library crate holds the whole agent: the CLI, the
//! capability composition, the [`agent::Agent`] turn driver, the
//! [`provider::Provider`] transport, and the two frontends
//! ([`tui::run`] / [`headless::run`]).  The `exarch` binary is a thin
//! shell over [`run`]; integration tests in `tests/` link this library
//! directly to drive [`agent::Agent::apply`] through a scripted
//! provider (see [`provider::Provider::scripted`]).
#![allow(
    clippy::disallowed_methods,
    reason = "exarch is an application, not the ral shell; the clippy.toml invariants target ral-core's Shell path/cwd/fs discipline"
)]
pub mod agent;
pub mod bootstrap;
pub mod bus;
pub mod cli;
pub mod config;
pub mod fleet;
pub mod headless;
pub mod policy;
pub mod prompt;
pub mod provider;
pub mod shell_eval;
pub mod tui;

use agent::Agent;
use clap::Parser;
use provider::{Engine, Provider};
use std::sync::Arc;
use tui::SessionInfo;

/// Pre-`main` trampoline shared by the binary and every test binary.
///
/// A byte-mode pipeline stage re-execs the running binary with
/// `--ral-pipeline-stage-helper`; under `cargo test` that is the test
/// harness binary, which libtest would reject.  Teach core how to dress
/// a sandbox-IPC child's fresh shell with exarch's host builtins, then
/// serve the helper dispatch before libtest sees the flag.  The binary
/// calls this from `main`; test binaries call it from a `#[ctor]`.
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

/// The full pre-`main` re-exec dispatch, shared by the binary's `main` and
/// every test `#[ctor]`.
///
/// Serves helper re-execs
/// ([`install_child_hooks_and_serve_helpers`] — which also sets the
/// child-shell extension), then the OS-sandbox stage
/// ([`ral_core::sandbox::serve_sandbox_early_init`]). Returns `Some(code)`
/// when this process is a re-exec child that should exit now, `None` to
/// continue to the frontend. `main` and the test ctors run this identical
/// function; the only difference is how they act on `Some` (return vs exit).
pub fn dispatch_pre_main() -> Option<u8> {
    install_child_hooks_and_serve_helpers().or_else(ral_core::sandbox::serve_sandbox_early_init)
}

/// Emit the pre-`main` re-exec `#[ctor]` shared by the binary and tests.
///
/// It serves helper / sandbox re-execs from a constructor and exits the
/// child before libtest sees flags it would reject. Invoked once per binary
/// (gate with `#[cfg(test)]` where the ctor is only wanted under test). See
/// [`dispatch_pre_main`].
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

// The library's own unit-test binary; see [`dispatch_pre_main`].
#[cfg(test)]
pre_main_ctor!();

/// The binary's entry point, lifted into the library so integration
/// tests can link the whole crate.
///
/// Parses the CLI, composes the
/// capability lattice, builds a [`Agent`] + [`Provider`], and hands
/// off to a frontend.
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
    // A subcommand runs its action and exits before any session setup —
    // notably before the provider-availability check below, since `login` is
    // how the OpenAI provider becomes available.
    if let Some(command) = c.command {
        return match command {
            cli::Command::Login { device_auth } => provider::oauth::login(device_auth),
            cli::Command::Logout { account, all } => provider::oauth::logout(account, all),
            cli::Command::Accounts => {
                let accounts = provider::oauth::load_all();
                if accounts.is_empty() {
                    eprintln!("No ChatGPT accounts signed in. Run `exarch login` to add one.");
                } else {
                    // Show the account id alongside the label only when the
                    // label is an email — otherwise the label *is* the id and
                    // printing it twice is noise.
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
    // Belt-and-suspenders to the `requires = "headless"` on `--output-format`
    // in cli.rs: `--output-format` only affects the headless frontend, so a
    // `json` request without `--headless` is a mistake, not a silent no-op.
    if c.output_format == headless::OutputFormat::Json && !c.headless {
        return Err("--output-format is only meaningful with --headless".into());
    }
    let seed = cli::load_seed(c.prompt, c.file, c.trailing_prompt)?;

    // Load the unusual-provider config (custom endpoints) from the trusted
    // XDG config home, evaluated under a no-authority grant. Absent → none.
    let custom = config::load()?;
    // The operator's disk-warn ceiling, if set — threaded to the trunk below.
    let disk_warn_bytes = config::disk_warn_bytes()?;

    // Auto-discover providers and resolve their keys into the in-memory
    // store, scrubbing every key var from the environment. The custom
    // providers join the famous ones in the same sweep.
    // SAFETY: startup is still single-threaded here — the tokio runtime
    // and the session's worker threads are created below — so no other
    // thread can race this env mutation. This is the only credential scrub;
    // every spawned child therefore inherits an environment free of keys.
    let mut store = provider::credential::CredentialStore::resolve_and_scrub(custom);
    let available = store.available();
    if available.is_empty() {
        return Err(
            "no provider available — set a provider API key (e.g. ANTHROPIC_API_KEY, \
             OPENAI_API_KEY, OPENROUTER_API_KEY, DEEPSEEK_API_KEY)"
                .into(),
        );
    }

    let cwd =
        std::env::current_dir().map_or_else(|_| ".".into(), |p| p.to_string_lossy().into_owned());
    let state_dir = bootstrap::EXARCH.project_dir(&cwd);

    // Resolve the initial selection: an explicit `--provider` pin, else an
    // explicit `--model` override, else the persisted selection (when its
    // provider is available), else the first available provider's default model.
    let mut catalog =
        provider::models::ModelCatalog::new(provider::models::LiveSource::new(&store));
    let (id, model, mut tuning, route) = resolve_initial_selection(
        c.provider.as_deref(),
        c.model.as_deref(),
        &state_dir,
        &available,
        &mut catalog,
    )?;
    if let Some(keyword) = c.effort.as_deref() {
        tuning.effort = Some(provider::ReasoningEffort::from_keyword(keyword).ok_or_else(
            || format!("invalid effort '{keyword}' — expected none|low|medium|high|xhigh|max"),
        )?);
    }
    // An unset model (the model-less launch of a custom provider with no
    // default) is only meaningful for the interactive frontend, which opens
    // the `/model` picker; a headless run has no picker and nothing to fall
    // back on, so it must be told a model explicitly.
    if model.is_empty() && c.headless {
        return Err(format!(
            "custom provider '{}' has no default model — pass --model NAME for a headless run",
            id.label()
        ));
    }
    // A `--model` override is a deliberate choice, so remember it: the next
    // launch in this project restores it as the persisted selection, exactly
    // as the `/model` picker's choice is remembered. (The empty-model launch
    // has nothing to persist.)
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
    // The model selection is exarch's own runtime state under the XDG state
    // home (`bootstrap::App::project_dir`), outside the agent's cwd sandbox, so a
    // tool call cannot reach it — no deny-list entry is needed.
    // RAL_DUMP_SANDBOX_PROFILE: emit the platform OS-sandbox profile
    // (Seatbelt SBPL, bwrap argv, or AppContainer summary) the launcher
    // would install for an external child under this projection.
    // A throwaway shell mirrors the stack.
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
    let run_dir = bootstrap::EXARCH
        .log_run_dir(&cwd)
        .map_err(|e| format!("log dir: {e}"))?;
    let config_dir = bootstrap::EXARCH.xdg_dir(ral_core::path::basedir::XdgKind::Config);
    let cwd_path = std::path::PathBuf::from(&cwd);
    // Chat mode registers no tools, so there is nothing for a system prompt to
    // describe: it is skipped entirely in favour of the minimal placeholder.
    let system = if c.chat {
        prompt::CHAT_SYSTEM.to_string()
    } else {
        prompt::assemble(
            &c.system_files,
            &caps,
            &scratch,
            &cwd_path,
            &config_dir,
            c.headless,
            c.edit,
        )?
    };
    let system_size = system.len();

    // One shared runtime for the whole fleet; per-credential transports warm
    // lazily as providers are built and borrow it.
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
            model,
            provider_label: label.to_string(),
            allow_schedule: c.allow_schedule,
            // The interactive (TUI) trunk converses and parks for the human;
            // a headless trunk terminates once its seeded work is idle.
            interactive: !c.headless,
            chat: c.chat,
            disk_warn_bytes,
        },
        agent::RootSeat::Identity {
            scratch: Arc::clone(&scratch),
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

/// Resolve the initial provider+model from, in priority order: an explicit
/// `--provider` pin; an explicit `--model` override (its provider resolved by
/// [`provider::models::resolve_model_provider`]); the persisted selection, when its
/// provider is still available; else the first available provider's default
/// model. The selection always names an *available* provider — a saved
/// selection naming a provider whose key is no longer set falls through to the
/// default rather than failing.
///
/// `--provider` pins the identity with no model-listing lookup and no
/// saved-state consult: with `--model` it takes the pair verbatim (the way to
/// reach a model the provider does not advertise), and alone it takes the
/// provider's default model.
///
/// A custom provider (or a `ChatGPT` account) has no built-in default model, so
/// when the selection would fall to one with no saved selection and no
/// `--model` — whether pinned by `--provider` or defaulted to — the user is
/// asked to name a model, since there is nothing to assume.
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
        // A custom provider has no built-in default model, but that is no
        // reason to refuse to launch: open with the model *unset* (the empty
        // sentinel) so the interactive frontend lands on the prompt with a
        // "run /model" hint and the human picks from the live catalog. The
        // headless frontend cannot open the picker, so `run` still rejects an
        // unset model there — see the guard below.
        None => Ok((id, String::new(), provider::Tuning::initial(), None)),
    }
}
