//! Command-line surface: clap's parse of argv, and the seed prompt resolved
//! out of `--prompt`/`--file`.  The system prompt is built by
//! `prompt::assemble`.

use crate::headless::OutputFormat;
use crate::prompt::EditScheme;
use crate::provider::identity::Login;
use crate::site::{Silent, site};
use clap::{ArgGroup, Parser, Subcommand};
use std::{sync::LazyLock, time::Duration};

static HELPER_IDLE_DEFAULT: LazyLock<String> =
    LazyLock::new(|| crate::schedule::fmt_duration(crate::agent::fleet::HELPER_IDLE));

/// All flags are long-form only: short letters would collide, and there are
/// few enough to spell out.
#[derive(Parser, Debug)]
#[command(
    about = "A coding agent based on the ral shell.",
    long_about = None,
    group(ArgGroup::new("headless_mode").args(["headless", "prompt"]).multiple(true))
)]
// Each bool is its own switch, not a field of some bundle worth grouping.
#[allow(clippy::struct_excessive_bools)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Choose the model that Exarch uses when it starts.
    ///
    /// The provider must list the model. If you do not also use `--provider`,
    /// Exarch uses the one available provider that lists it, and asks you to
    /// name one if several do. If you omit this option, Exarch restores the
    /// model saved for the current project; with nothing saved, it asks you to
    /// pick one before the session starts. A headless run with nothing to
    /// restore needs this option. The model a run starts with is saved for
    /// the next run.
    #[arg(long)]
    pub model: Option<String>,
    /// Choose the upstreams that serve `--model` on a routing service.
    ///
    /// Give the endpoint tags `OpenRouter` lists for the model, such as
    /// `deepinfra/fp8`, separated by commas, most preferred first. Every request
    /// goes to one of them and never elsewhere: when none can serve it, the request
    /// fails rather than fall back. Without this option the service chooses an
    /// upstream for each request. Exarch checks each tag against the model's
    /// endpoints when it starts, and saves the choice with the model.
    #[arg(
        long,
        value_name = "TAGS",
        value_parser = crate::provider::Via::parse,
        requires = "model"
    )]
    pub via: Option<crate::provider::Via>,
    /// Choose the provider or signed-in account that Exarch uses when it starts.
    ///
    /// Give an account ID, a service name such as `anthropic` or `openai`, or
    /// an account exactly as `exarch accounts` lists it. Exarch asks you to be more
    /// specific if the name matches several accounts. Without `--model`,
    /// Exarch restores the model saved for this account in the current
    /// project, or else asks you to pick one of its models before the session
    /// starts.
    #[arg(long)]
    pub provider: Option<String>,
    /// Bind a role that a spawned agent may run on.
    ///
    /// Give NAME=MODEL and repeat this option for more roles. A child started
    /// with model `role 'NAME'` runs on MODEL, with this run's `--effort` and
    /// `--max-tokens`. MODEL is resolved as `--model` is: to the one account
    /// that lists it, or to the one that `--role-provider` names, and served
    /// via the upstreams that `--role-via` names when given. Without roles,
    /// every child runs on its parent's model. Roles last for this run only
    /// and are not saved.
    #[arg(
        long,
        value_name = "NAME=MODEL",
        value_parser = crate::provider::roles::binding,
        conflicts_with = "chat"
    )]
    pub role: Vec<(String, String)>,
    /// Choose the account that a role's model runs on.
    ///
    /// Give NAME=ACCOUNT, with ACCOUNT as `--provider` accepts it. You need
    /// this only when several accounts list the role's model.
    #[arg(
        long = "role-provider",
        value_name = "NAME=ACCOUNT",
        value_parser = crate::provider::roles::binding,
        conflicts_with = "chat"
    )]
    pub role_provider: Vec<(String, String)>,
    /// Choose the upstreams that serve a role's model.
    ///
    /// Give NAME=TAGS, with TAGS as `--via` takes them.
    #[arg(
        long = "role-via",
        value_name = "NAME=TAGS",
        value_parser = crate::provider::roles::via_binding,
        conflicts_with = "chat"
    )]
    pub role_via: Vec<(String, crate::provider::Via)>,
    /// Run one headless exchange with this prompt.
    ///
    /// This implies `--headless`. Exarch passes the text to the model exactly
    /// as written. Empty text or text containing only spaces counts as no
    /// prompt. You cannot give a prompt as an unnamed argument.
    #[arg(long, conflicts_with = "file")]
    pub prompt: Option<String>,
    /// Read the opening prompt from a file.
    ///
    /// Exarch reads the whole file as text. An empty file or one containing
    /// only spaces counts as no prompt. A headless run needs a non-blank
    /// prompt. You cannot use this option with `--prompt`.
    #[arg(long)]
    pub file: Option<std::path::PathBuf>,
    /// Replace Exarch's default persona with text from one or more files.
    ///
    /// Repeat this option to join files in command-line order, with a blank
    /// line between them. This replaces only the opening persona; Exarch still
    /// adds its ral, editing, tool, task, workspace and skill instructions. You
    /// cannot use this option in chat mode.
    #[arg(long = "system", value_name = "FILE")]
    pub system_files: Vec<std::path::PathBuf>,
    /// Choose the starting permission set for the agent.
    ///
    /// Use `reasonable` for normal work, `edit-only` to allow edits but not
    /// builds, `read-only` to keep writes in scratch space, `minimal` for
    /// system tools, the working directory and network access, or `confined`
    /// for an offline build jail. `confined`, `minimal`, `read-only` and
    /// `edit-only` also withhold the GPU, on Linux and macOS; Windows cannot
    /// withhold it. `dangerous` places no restriction on the
    /// agent. Once commands are restricted, exarch excludes its own binary;
    /// once the filesystem is, its credential files and every `--restrict`
    /// file. Use it only inside another trusted boundary. The default is
    /// `reasonable`.
    #[arg(long = "base", value_name = "NAME", default_value = "reasonable")]
    pub base: String,
    /// Join one ral file's allowances and exclusions to the starting permission set.
    ///
    /// Exarch applies this file before any `--restrict` files. An allowance
    /// never removes permission, and an exclusion always wins. An area the
    /// base leaves unrestricted stays so, less the file's exclusions. Exarch
    /// never discovers this file automatically; you must name it explicitly.
    #[arg(long = "extend", value_name = "FILE")]
    pub extend: Option<std::path::PathBuf>,
    /// Reduce the agent's permissions using a ral file.
    ///
    /// Repeat this option to apply more restrictions, in argument order: a
    /// restriction can never add permission, but what a run allowance makes
    /// readable reaches only the restrictions applied after it. Exarch also denies
    /// the agent access to each restriction file, so the agent cannot change
    /// its own permissions.
    #[arg(long = "restrict", value_name = "FILE")]
    pub restrict: Vec<std::path::PathBuf>,
    /// Limit the number of output tokens in each model request.
    ///
    /// If you omit this option, Exarch sends no limit and the provider chooses
    /// its own default.
    #[arg(long = "max-tokens", value_name = "N")]
    pub max_tokens: Option<u32>,
    /// Choose how much reasoning effort the model may use.
    ///
    /// The choices are `auto`, `zero`, `low`, `med`, `high`, `xhigh` and
    /// `max`. Availability depends on the model and provider. `auto` sends no
    /// explicit setting, so the provider decides. An explicit choice is saved
    /// for the next run in the current project.
    #[arg(long = "effort", value_name = "RUNG")]
    pub effort: Option<String>,
    /// Run one prompted exchange without opening the terminal interface.
    ///
    /// Give the opening prompt with `--prompt` or `--file`. Exarch writes the
    /// agent's final reply to standard output, writes progress and other events
    /// to standard error, and exits when the exchange has finished.
    #[arg(long)]
    pub headless: bool,
    /// Continue a recorded run instead of starting a new one.
    ///
    /// With no TARGET, Exarch chooses the newest unlocked run for the current
    /// working directory. You can instead name a run directory or its
    /// `sessions/0` directory. A headless resume still needs a new opening
    /// prompt. You cannot resume in chat mode.
    #[arg(long, value_name = "TARGET", num_args = 0..=1, conflicts_with = "chat")]
    pub resume: Option<Option<std::path::PathBuf>>,
    /// Choose the format written to standard output in headless mode.
    ///
    /// `text`, the default, writes the agent's final reply as readable ral
    /// text. `json` writes one result object containing the reply, stop reason,
    /// step count, duration, token use and cost. This option requires
    /// headless mode, selected by `--headless` or implied by `--prompt`.
    #[arg(long = "output-format", value_enum, default_value_t = OutputFormat::Text, requires = "headless_mode")]
    pub output_format: OutputFormat,
    /// Allow the agent to schedule its own future wake-ups.
    ///
    /// This gives the agent access to one-off delays and recurring schedules.
    /// A scheduled agent can keep a run alive and continue working without a
    /// new prompt, so grant this permission deliberately. It is unavailable in
    /// chat mode.
    #[arg(long = "allow-schedule", conflicts_with = "chat")]
    pub allow_schedule: bool,

    /// How long a helper agent rests, with nothing from its parent, before it ends.
    #[arg(
        long,
        value_name = "DURATION",
        value_parser = crate::schedule::parse_duration,
        default_value = HELPER_IDLE_DEFAULT.as_str()
    )]
    pub helper_idle: Duration,

    /// Use vi-style keys to edit prompts in the terminal interface.
    ///
    /// The editor starts in insert mode. Without this option, it uses
    /// Emacs-style keys. This option is unavailable in headless mode.
    #[arg(long = "vi", conflicts_with = "headless_mode")]
    pub vi: bool,

    /// Choose the file-editing method that Exarch teaches the agent to use.
    ///
    /// `hash` identifies lines by hashes before changing them.
    /// `replace` replaces exact text. Both methods remain available as tools;
    /// this option changes the instructions, not the installed tools. It is
    /// unavailable in chat mode.
    #[arg(long = "edit", value_enum, default_value_t = EditScheme::Hash, conflicts_with = "chat")]
    pub edit: EditScheme,

    /// Start a simple interactive conversation with the model.
    ///
    /// Chat mode gives the model no tools and none of Exarch's normal agent
    /// instructions. Exarch still records the conversation. You cannot combine
    /// chat mode with `--headless`, `--prompt`, `--system`, `--resume`,
    /// `--allow-schedule` or `--edit`.
    #[arg(long = "chat", conflicts_with_all = ["headless_mode", "system_files"])]
    pub chat: bool,

    /// Also offer the `thinking` relay — an experiment, so kept off `--help`.
    #[arg(long = "thinking-tool", hide = true, conflicts_with = "chat")]
    pub thinking_tool: bool,
}

impl Cli {
    /// Whether this invocation uses the one-shot frontend.
    pub(crate) fn is_headless(&self) -> bool {
        self.headless || self.prompt.is_some()
    }
}

/// An out-of-band action that runs and exits instead of starting a session.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Sign in to a plan: openai-chatgpt, openai-codex or xai-grok.
    ///
    /// Add a new account, or name an account of this service to renew its
    /// sign-in. Choose between signed-in accounts with `/model`. `/login`
    /// offers the same flows inside a live session.
    Login {
        /// The sign-in service: openai-chatgpt, openai-codex or xai-grok.
        #[arg(value_parser = Login::named)]
        service: Login,
        /// An account to sign in again, by its email address or account ID.
        account: Option<String>,
        /// Use a device code instead of a browser (Codex and Grok only).
        #[arg(long)]
        device_auth: bool,
    },
    /// Remove a stored plan login.
    ///
    /// Name an account, use `--all`, or leave both out. With neither, Exarch
    /// removes the account if exactly one is signed in; otherwise it asks you
    /// which account to remove.
    Logout {
        /// Choose the account to sign out by its email address or account ID.
        account: Option<String>,
        /// Sign out of every stored account, across all sign-in services.
        ///
        /// You cannot use this option while also naming one account.
        #[arg(long, conflicts_with = "account")]
        all: bool,
    },
    /// List the accounts signed in to Exarch, across all sign-in services.
    Accounts,
    /// Render a session's transcript as text.
    ///
    /// Give a session directory or its `record.jsonl`. Exarch writes the
    /// transcript as the scrollback showed it, to standard output unless you
    /// use `--out`.
    Export {
        /// A session directory, or the `record.jsonl` inside one.
        session: std::path::PathBuf,
        /// Write the transcript to this file instead of standard output.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
}

/// Resolve `--prompt/--file` into an optional initial prompt.
///
/// A blank seed (empty or whitespace-only) collapses to `None` here so the
/// frontends need not each re-filter: headless rejects a missing seed, the TUI
/// just opens to a prompt.
///
/// # Errors
/// Returns `Err` if the `--file` seed prompt cannot be read.
pub fn load_seed(
    prompt: Option<String>,
    file: Option<std::path::PathBuf>,
) -> Result<Option<String>, String> {
    let seed = match (prompt, file) {
        (Some(p), _) => Some(p),
        (None, Some(path)) => Some(site!(
            Silent::CliArgument,
            ral_core::own::read_named(&path)
        )?),
        (None, None) => None,
    };
    Ok(seed.filter(|s| !s.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command};
    use crate::provider::identity::Login;
    use clap::Parser;

    #[test]
    fn helper_idle_defaults_to_five_hours() {
        let cli = Cli::try_parse_from(["exarch"]).expect("default helper idle");
        assert_eq!(cli.helper_idle, std::time::Duration::from_hours(5));
    }

    #[test]
    fn helper_idle_accepts_duration_units() {
        for (text, seconds) in [("45s", 45), ("30m", 1800), ("2h", 7200), ("1d", 86_400)] {
            let cli = Cli::try_parse_from(["exarch", "--helper-idle", text])
                .expect("positive helper idle duration");
            assert_eq!(cli.helper_idle, std::time::Duration::from_secs(seconds));
        }
    }

    #[test]
    fn helper_idle_rejects_missing_zero_malformed_and_overflowing_durations() {
        assert!(Cli::try_parse_from(["exarch", "--helper-idle"]).is_err());
        for text in ["0s", "0h", "10", "2x", "h", "", "18446744073709551615d"] {
            let error = Cli::try_parse_from(["exarch", "--helper-idle", text])
                .expect_err("invalid helper idle duration");
            assert!(error.to_string().contains("--helper-idle"), "{error}");
        }
    }

    #[test]
    fn prompt_is_headless_mode() {
        assert!(
            !Cli::try_parse_from(["exarch"])
                .expect("bare TUI")
                .is_headless()
        );

        let cli = Cli::try_parse_from(["exarch", "--prompt", "do it"])
            .expect("a prompt is a complete headless invocation");
        assert!(cli.is_headless());

        assert!(Cli::try_parse_from(["exarch", "--prompt", "do it", "--vi"]).is_err());
        assert!(Cli::try_parse_from(["exarch", "--prompt", "do it", "--chat"]).is_err());
        assert!(
            Cli::try_parse_from(["exarch", "--prompt", "do it", "--output-format", "json",])
                .is_ok()
        );
    }

    #[test]
    fn login_requires_an_explicit_service_without_aliases() {
        assert!(Cli::try_parse_from(["exarch", "login"]).is_err());
        for name in ["chatgpt", "codex", "grok", "openai-chatgp", "openai"] {
            assert!(
                Cli::try_parse_from(["exarch", "login", name]).is_err(),
                "{name}"
            );
        }
        for login in Login::ALL {
            let service = login.service();
            let cli = Cli::try_parse_from(["exarch", "login", service.name.as_str()])
                .expect("known service");
            assert!(matches!(cli.command, Some(Command::Login {
                service, account: None, device_auth: false,
            }) if service == login));
        }
    }

    #[test]
    fn login_device_flag_can_surround_the_positionals() {
        for args in [
            vec![
                "exarch",
                "login",
                "--device-auth",
                "openai-codex",
                "work@example.test",
            ],
            vec![
                "exarch",
                "login",
                "openai-codex",
                "--device-auth",
                "work@example.test",
            ],
            vec![
                "exarch",
                "login",
                "openai-codex",
                "work@example.test",
                "--device-auth",
            ],
        ] {
            let cli = Cli::try_parse_from(args).expect("device renewal grammar");
            assert!(matches!(cli.command, Some(Command::Login {
                service: Login::Codex, account: Some(account), device_auth: true,
            }) if account == "work@example.test"));
        }
        // Method support is checked before network work in login_flow, not clap.
        for service in ["openai-chatgpt", "xai-grok"] {
            assert!(Cli::try_parse_from(["exarch", "login", service, "--device-auth"]).is_ok());
        }
    }

    #[test]
    fn logout_exclusivity_and_accounts_grammar_are_unchanged() {
        assert!(Cli::try_parse_from(["exarch", "logout"]).is_ok());
        assert!(Cli::try_parse_from(["exarch", "logout", "person@example.test"]).is_ok());
        assert!(Cli::try_parse_from(["exarch", "logout", "--all"]).is_ok());
        assert!(Cli::try_parse_from(["exarch", "logout", "person@example.test", "--all"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["exarch", "accounts"])
                .expect("account listing")
                .command,
            Some(Command::Accounts)
        ));
        assert!(Cli::try_parse_from(["exarch", "accounts", "openai-codex"]).is_err());
    }
}
