//! Exarch — a delegate driving ral in process under a user-chosen grant
//! policy.  This library crate holds the whole agent: the CLI, the
//! capability composition, the [`session::Session`] turn driver, the
//! [`provider::Provider`] transport, and the two frontends
//! ([`tui::run`] / [`headless::run`]).  The `exarch` binary is a thin
//! shell over [`run`]; integration tests in `tests/` link this library
//! directly to drive [`session::Session::apply`] through a scripted
//! provider (see [`provider::Provider::scripted`]).

pub mod agent_builtins;
pub mod agent_registry;
pub mod bootstrap;
pub mod bus;
pub mod cancel;
pub mod card;
pub mod cli;
pub mod config;
pub mod credential;
pub mod digest;
pub mod event;
pub mod headless;
pub mod host;
pub mod models;
pub mod nudge;
pub mod oauth;
pub mod policy;
pub mod pricing;
pub mod prompt;
pub mod provider;
pub mod schedule;
pub mod session;
pub mod shell_eval;
pub mod state;
pub mod tls;
pub mod tools;
pub mod tui;

use clap::Parser;
use provider::Provider;
use session::Session;
use tui::SessionInfo;

/// Pre-`main` trampoline shared by the binary and every test binary.
/// A byte-mode pipeline stage re-execs the running binary with
/// `--ral-pipeline-stage-helper`; under `cargo test` that is the test
/// harness binary, which libtest would reject.  Teach core how to dress
/// a sandbox-IPC child's fresh shell with exarch's host builtins, then
/// serve the helper dispatch before libtest sees the flag.  The binary
/// calls this from `main`; test binaries call it from a `#[ctor]`.
pub fn install_child_hooks_and_serve_helpers() -> Option<u8> {
    ral_core::sandbox::set_child_shell_extension(|shell| {
        agent_builtins::install_on(shell);
    });
    if let Some(code) = ral_core::try_run_pipeline_stage_helper() {
        return Some(code);
    }
    if let Some(code) = ral_core::test_helper::try_run_test_helper() {
        return Some(code);
    }
    None
}

/// The full pre-`main` re-exec dispatch, shared by the binary's `main` and
/// every test `#[ctor]`: serve helper re-execs
/// ([`install_child_hooks_and_serve_helpers`] — which also sets the
/// child-shell extension), then the OS-sandbox stage
/// ([`ral_core::sandbox::serve_sandbox_early_init`]). Returns `Some(code)`
/// when this process is a re-exec child that should exit now, `None` to
/// continue to the frontend. `main` and the test ctors run this identical
/// function; the only difference is how they act on `Some` (return vs exit).
pub fn dispatch_pre_main() -> Option<u8> {
    install_child_hooks_and_serve_helpers().or_else(ral_core::sandbox::serve_sandbox_early_init)
}

/// Pre-`main` trampoline for the library's own unit-test binary; see
/// [`dispatch_pre_main`].
#[cfg(test)]
#[ctor::ctor(unsafe)]
fn init_lib_test_binary() {
    if let Some(code) = dispatch_pre_main() {
        std::process::exit(code as i32);
    }
}

/// The binary's entry point, lifted into the library so integration
/// tests can link the whole crate.  Parses the CLI, composes the
/// capability lattice, builds a [`Session`] + [`Provider`], and hands
/// off to a frontend.
pub fn run() -> Result<(), String> {
    let c = cli::Cli::parse();
    // A subcommand runs its action and exits before any session setup —
    // notably before the provider-availability check below, since `login` is
    // how the OpenAI provider becomes available.
    if let Some(command) = c.command {
        return match command {
            cli::Command::Login { device_auth } => oauth::login(device_auth),
            cli::Command::Logout { account, all } => oauth::logout(account, all),
            cli::Command::Accounts => {
                let accounts = oauth::load_all();
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
    // Belt-and-suspenders to the clap `requires` above: `--output-format`
    // only affects the headless frontend, so a `json` request without
    // `--headless` is a mistake, not a silent no-op.
    if c.output_format == headless::OutputFormat::Json && !c.headless {
        return Err("--output-format is only meaningful with --headless".into());
    }
    let seed = cli::load_seed(c.prompt, c.file)?;

    // Load the unusual-provider config (custom endpoints) from the trusted
    // XDG config home, evaluated under a no-authority grant. Absent → none.
    let custom = config::load()?;

    // Auto-discover providers and resolve their keys into the in-memory
    // store, scrubbing every key var from the environment. The custom
    // providers join the famous ones in the same sweep.
    // SAFETY: startup is still single-threaded here — the tokio runtime
    // and the session's worker threads are created below — so no other
    // thread can race this env mutation. This is the only credential scrub;
    // every spawned child therefore inherits an environment free of keys.
    let store = credential::CredentialStore::resolve_and_scrub(custom);
    let available = store.available();
    if available.is_empty() {
        return Err(
            "no provider available — set a provider API key (e.g. ANTHROPIC_API_KEY, \
             OPENAI_API_KEY, OPENROUTER_API_KEY, DEEPSEEK_API_KEY)"
                .into(),
        );
    }

    #[allow(clippy::disallowed_methods)]
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());
    let state_dir = bootstrap::project_dir(&cwd);

    // Resolve the initial selection: an explicit `--model` override, else
    // the persisted selection (when its provider is available),
    // else the first available provider's default model.
    let mut catalog = models::ModelCatalog::new(models::LiveSource::new(&store));
    let (id, model) =
        resolve_initial_selection(c.model.as_deref(), &state_dir, &available, &mut catalog)?;
    let label = id.label();
    let cred = store
        .get(&id)
        .expect("selected provider must be available")
        .clone();

    let (caps, restrict_files) =
        policy::for_invocation(&cwd, &c.base, c.extend_base.as_deref(), &c.restrict)?;
    // The model selection is exarch's own runtime state under the XDG state
    // home (`bootstrap::project_dir`), outside the agent's cwd sandbox, so a
    // tool call cannot reach it — no deny-list entry is needed.
    // RAL_DUMP_SANDBOX_PROFILE: emit the SBPL the per-command sandbox
    // launcher will install for an external child under this projection.
    // A throwaway shell mirrors the stack.
    {
        let mut probe = ral_core::Shell::new(Default::default());
        probe.push_session_capabilities(caps.clone());
        if let Some(projection) = probe.sandbox_projection() {
            ral_core::sandbox::dump_profile_if_requested(&projection);
        }
    }
    let scratch = bootstrap::Scratch::new().map_err(|e| format!("scratch dir: {e}"))?;
    let run_dir = bootstrap::log_run_dir(&cwd).map_err(|e| format!("log dir: {e}"))?;
    let system = prompt::assemble(&c.system_files, &caps, scratch.path(), c.headless)?;
    let system_size = system.len();

    // Behind an `Arc` from the start: an async `agent` worker captures a
    // clone to outlive its spawning turn, and a `/model` switch swaps this
    // for a fresh one without disturbing children already running.
    let provider = std::sync::Arc::new(Provider::build(&id, model.clone(), &cred, c.max_tokens));
    let mut session = Session::root(
        system,
        caps,
        &scratch,
        &run_dir,
        &model,
        label,
        c.expect_action,
        c.allow_schedule,
        // Interactive (TUI) roots park for the human; a headless root
        // terminates once its seeded work is idle.
        !c.headless,
    )
    .map_err(|e| format!("session init: {e}"))?;

    let provider_caps = provider::caps_for(provider.model());
    let info = SessionInfo {
        provider: label,
        model: &model,
        canonical_slug: provider_caps.canonical_slug.as_deref(),
        max_tokens_override: provider.max_tokens_override(),
        context_window: provider_caps.context_window,
        max_output_tokens: provider_caps.max_output_tokens,
        system_size,
        system_files: &c.system_files,
        base: &c.base,
        extend_base: c.extend_base.as_deref(),
        restrict_files: &restrict_files,
        scratch: scratch.path(),
        cwd: &cwd,
    };
    if c.headless {
        headless::run(&mut session, &provider, &info, seed, c.output_format)
    } else {
        tui::run(
            &mut session,
            provider,
            &info,
            &store,
            &mut catalog,
            &scratch,
            &run_dir,
            seed,
            c.vi,
        )
    }
}

/// Resolve the initial provider+model from, in priority order: an explicit
/// `--model` override (its provider resolved by [`models::resolve_model_provider`]);
/// the persisted selection, when its provider is still available;
/// else the first available provider's default model. The selection always
/// names an *available* provider — a saved selection naming a provider whose
/// key is no longer set falls through to the default rather than failing.
///
/// A custom provider has no built-in default model (its `config.ral` declares
/// only the endpoint, key, and protocol), so when the default would fall to a
/// custom provider with no saved selection and no `--model`, the user is
/// asked to name a model — there is nothing to assume.
fn resolve_initial_selection(
    model_override: Option<&str>,
    state_dir: &std::path::Path,
    available: &[provider::ProviderId],
    catalog: &mut models::ModelCatalog<models::LiveSource>,
) -> Result<(provider::ProviderId, String), String> {
    if let Some(name) = model_override {
        let id = models::resolve_model_provider(name, available, catalog)?;
        return Ok((id, name.to_string()));
    }
    if let Some(saved) = state::load(state_dir)
        && let Some(id) = saved.provider_id(available)
    {
        return Ok((id, saved.model));
    }
    let id = available[0].clone();
    match id.famous() {
        Some(kind) => Ok((id, kind.info().1.to_string())),
        None => Err(format!(
            "custom provider '{}' has no default model — pass --model NAME \
             (it will be remembered) or open the /model picker",
            id.label()
        )),
    }
}
