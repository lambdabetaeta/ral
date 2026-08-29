//! Command-line surface: clap's parse of argv, and the seed prompt resolved
//! out of `--prompt`/`--file`.  The system prompt is built by
//! `prompt::assemble`.

use crate::headless::OutputFormat;
use clap::{Parser, Subcommand};

/// All flags are long-form only: short letters would collide, and there are
/// few enough to spell out.
#[derive(Parser, Debug)]
#[command(about = "Exarch — a delegate driving ral under a grant", long_about = None)]
// Each bool is its own switch, not a field of some bundle worth grouping.
#[allow(clippy::struct_excessive_bools)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Choose the model that Exarch uses when it starts.
    ///
    /// If you do not also use `--provider`, Exarch looks for one available
    /// provider that lists this model. A `vendor/model` name can fall back to a
    /// routing provider, and a bare name can fall back to the only available
    /// provider. If you omit this option, Exarch restores the model saved for
    /// the current project or uses the first provider's default. An explicit
    /// choice is saved for the next run.
    #[arg(long)]
    pub model: Option<String>,
    /// Choose the provider or signed-in account that Exarch uses when it starts.
    ///
    /// Give an account ID, a service name such as `anthropic` or `openai`, or
    /// an account handle shown by `exarch accounts`. Exarch asks you to be more
    /// specific if the name matches several accounts. With `--model`, Exarch
    /// uses the pair exactly as given, even if the provider does not list that
    /// model. Without `--model`, Exarch uses the provider's default. An explicit
    /// choice is saved for the next run.
    #[arg(long)]
    pub provider: Option<String>,
    /// Start the session with this prompt.
    ///
    /// Exarch passes the text to the model exactly as written. Empty text or
    /// text containing only spaces counts as no prompt. A headless run needs a
    /// non-blank prompt. You cannot give a prompt as an unnamed argument.
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
    /// for an offline build jail. `dangerous` places no restriction on the
    /// agent and should only be used inside another trusted boundary. The
    /// default is `reasonable`.
    #[arg(long = "base", value_name = "NAME", default_value = "reasonable")]
    pub base: String,
    /// Add permissions from one ral file to the starting permission set.
    ///
    /// Exarch applies this file before any `--restrict` files, so it can make
    /// the selected base more permissive. Exarch never discovers this file
    /// automatically; you must name it explicitly.
    #[arg(long = "extend-base", value_name = "FILE")]
    pub extend_base: Option<std::path::PathBuf>,
    /// Reduce the agent's permissions using a ral file.
    ///
    /// Repeat this option to apply more restrictions. Their order does not
    /// matter, and a restriction can never add permission. Exarch also denies
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
    /// prompt. You cannot resume in chat mode or when using `--no-logs`.
    #[arg(long, value_name = "TARGET", num_args = 0..=1, conflicts_with_all = ["no_logs", "chat"])]
    pub resume: Option<Option<std::path::PathBuf>>,
    /// Keep the conversation record in memory instead of writing session logs.
    ///
    /// The session cannot be resumed after Exarch exits. You cannot use this
    /// option with `--resume`.
    #[arg(long = "no-logs")]
    pub no_logs: bool,
    /// Choose the format written to standard output in headless mode.
    ///
    /// `text`, the default, writes the agent's final reply as readable ral
    /// text. `json` writes one result object containing the reply, stop reason,
    /// step count, duration, token use and cost. This option requires
    /// `--headless`.
    #[arg(long = "output-format", value_enum, default_value_t = OutputFormat::Text, requires = "headless")]
    pub output_format: OutputFormat,
    /// Allow the agent to schedule its own future wake-ups.
    ///
    /// This gives the agent access to one-off delays and recurring schedules.
    /// A scheduled agent can keep a run alive and continue working without a
    /// new prompt, so grant this permission deliberately. It is unavailable in
    /// chat mode.
    #[arg(long = "allow-schedule", conflicts_with = "chat")]
    pub allow_schedule: bool,

    /// Use vi-style keys to edit prompts in the terminal interface.
    ///
    /// The editor starts in insert mode. Without this option, it uses
    /// Emacs-style keys. This option is unavailable in headless mode.
    #[arg(long = "vi", conflicts_with = "headless")]
    pub vi: bool,

    /// Choose the file-editing method that Exarch teaches the agent to use.
    ///
    /// `replace`, the default, replaces exact text. `hash` identifies lines by
    /// hashes before changing them. Both methods remain available as tools;
    /// this option changes the instructions, not the installed tools. It is
    /// unavailable in chat mode.
    #[arg(long = "edit", value_enum, default_value_t = EditScheme::Replace, conflicts_with = "chat")]
    pub edit: EditScheme,

    /// Start a simple interactive conversation with the model.
    ///
    /// Chat mode gives the model no tools and none of Exarch's normal agent
    /// instructions. Exarch still records the conversation unless you use
    /// `--no-logs`. You cannot combine chat mode with `--headless`, `--system`,
    /// `--resume`, `--allow-schedule` or `--edit`.
    #[arg(long = "chat", conflicts_with_all = ["headless", "system_files"])]
    pub chat: bool,
}

/// The editing scheme `--edit` selects: one system-prompt section, since both
/// editing builtins are registered regardless.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum EditScheme {
    /// Teach the agent to inspect text and replace an exact string. This is the
    /// default.
    Replace,
    /// Teach the agent to inspect line hashes and make changes tied to the
    /// lines it inspected.
    Hash,
}

/// An out-of-band action that runs and exits instead of starting a session.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Sign in to `ChatGPT` and add the account as a model provider.
    ///
    /// You can sign in to several accounts and choose between them with
    /// `/model`. Use this command on a machine where you cannot first open an
    /// Exarch session; `/login` provides the same sign-in flow inside a live
    /// session.
    Login {
        /// Print a sign-in address and device code instead of opening a browser.
        ///
        /// Use this on a remote or text-only machine. Open the address on
        /// another device, then enter the displayed code.
        #[arg(long = "device-auth")]
        device_auth: bool,
    },
    /// Remove a stored `ChatGPT` login.
    ///
    /// Name an account, use `--all`, or leave both out. With neither, Exarch
    /// removes the account if exactly one is signed in; otherwise it asks you
    /// which account to remove.
    Logout {
        /// Choose the account to sign out by its email address or account ID.
        account: Option<String>,
        /// Sign out of every stored `ChatGPT` account.
        ///
        /// You cannot use this option while also naming one account.
        #[arg(long, conflicts_with = "account")]
        all: bool,
    },
    /// List the `ChatGPT` accounts that are signed in to Exarch.
    Accounts,
}

/// Resolve `--prompt/--file` into an optional initial prompt.
///
/// A blank seed (empty or whitespace-only) collapses to `None` here so the
/// frontends need not each re-filter: headless rejects a missing seed, the TUI
/// just opens to a prompt.
///
/// # Errors
/// Returns `Err` if the `--file` seed prompt cannot be read.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:seed-file] reads the --file seed prompt at CLI parse time; not a turn-time door"
)]
pub fn load_seed(
    prompt: Option<String>,
    file: Option<std::path::PathBuf>,
) -> Result<Option<String>, String> {
    let seed = match (prompt, file) {
        (Some(p), _) => Some(p),
        (None, Some(path)) => {
            Some(std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?)
        }
        (None, None) => None,
    };
    Ok(seed.filter(|s| !s.trim().is_empty()))
}
