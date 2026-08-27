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
    /// Initial model, for headless or scripted runs; interactively, `/model`
    /// picks one.  Its provider is the available one that lists the name, else
    /// `OpenRouter` for a `vendor/model` slug, else the sole provider
    /// available.  Unset: the saved selection, or the first provider's default.
    #[arg(long)]
    pub model: Option<String>,
    /// Pin the provider by label — `anthropic`, `openai`, a `config.ral` key,
    /// a signed-in `ChatGPT` account — skipping the model-listing resolution
    /// `--model` does alone.  With `--model` the pair is taken verbatim,
    /// reaching models the provider does not advertise; alone, its default.
    #[arg(long)]
    pub provider: Option<String>,
    /// The seed prompt, verbatim.  There is no bare-word form: a stray
    /// argument is a mistake, not a prompt.
    #[arg(long, conflicts_with = "file")]
    pub prompt: Option<String>,
    #[arg(long)]
    pub file: Option<std::path::PathBuf>,
    #[arg(long = "system", value_name = "FILE")]
    pub system_files: Vec<std::path::PathBuf>,
    /// Agent ceiling — one of six bake-ins: `dangerous` (no attenuation),
    /// `reasonable` (default; everyday tooling), `edit-only` (editing, no
    /// build), `read-only` (writes to scratch only), `minimal` (coreutils +
    /// cwd + net), `confined` (offline build jail).  Widen one with
    /// `--extend-base`, or start from `dangerous` and cut with `--restrict`.
    #[arg(long = "base", value_name = "NAME", default_value = "reasonable")]
    pub base: String,
    /// One ral file joined into the base *before* any attenuation, widening
    /// the ceiling.  It grants, so you name it: nothing is loaded from cwd.
    #[arg(long = "extend-base", value_name = "FILE")]
    pub extend_base: Option<std::path::PathBuf>,
    /// Attenuation file(s) met with the (possibly extended) base: repeatable,
    /// and order-free since meet is commutative.  Each file's own path joins
    /// the fs deny list, so the agent cannot rewrite its own permissions.
    #[arg(long = "restrict", value_name = "FILE")]
    pub restrict: Vec<std::path::PathBuf>,
    /// Per-request visible-output ceiling (`max_tokens` in the API call).
    /// Unset, none is sent and the provider's own default stands.
    #[arg(long = "max-tokens", value_name = "N")]
    pub max_tokens: Option<u32>,
    /// Reasoning effort, named by the rung of the `/model` picker's ladder:
    /// `auto`, `zero`, `low`, `med`, `high`, `xhigh`, `max`.  `auto` sends no
    /// option, and so is how a persisted rung is cleared.
    #[arg(long = "effort", value_name = "RUNG")]
    pub effort: Option<String>,
    /// Run one seed exchange non-interactively: the root's deliberate `reply`
    /// is rendered once to stdout at completion, every other event condenses
    /// to stderr, and the process exits when the exchange finishes. Requires a
    /// seed prompt.
    #[arg(long)]
    pub headless: bool,
    /// Reopen the newest run, or the named run/session directory.
    #[arg(long, value_name = "TARGET", num_args = 0..=1, conflicts_with_all = ["no_logs", "chat"])]
    pub resume: Option<Option<std::path::PathBuf>>,
    /// Keep only the in-memory model mirror; no durable records or run lock
    /// are created, and the session cannot be resumed.
    #[arg(long = "no-logs")]
    pub no_logs: bool,
    /// Headless stdout format. `text` (default) renders the deliberate reply
    /// once as human-readable ral text; `json` emits one result object — reply,
    /// stop reason, steps, duration, usage, cost — when the run ends.
    #[arg(long = "output-format", value_enum, default_value_t = OutputFormat::Text, requires = "headless")]
    pub output_format: OutputFormat,
    /// Authorise the agent to schedule its own wakeups (the `schedules`
    /// enquiry) — waking itself indefinitely is authority.
    #[arg(long = "allow-schedule", conflicts_with = "chat")]
    pub allow_schedule: bool,

    /// Edit the TUI prompt in vi mode rather than emacs — the `prompt-editor`
    /// state machine the ral worksheet drives too.  It opens in insert mode.
    #[arg(long = "vi", conflicts_with = "headless")]
    pub vi: bool,

    /// Which file-editing scheme the system prompt teaches: `replace` (the
    /// default) literal string replacement, `hash` the witnessed line-hash one.
    /// Both stay registered, so the untaught one works if named.
    #[arg(long = "edit", value_enum, default_value_t = EditScheme::Replace, conflicts_with = "chat")]
    pub edit: EditScheme,

    /// Chat mode: no tools, no system prompt — a bare back-and-forth with the
    /// model.  A lone `.` stands in for the prompt, since some backends reject
    /// an empty system prompt and Anthropic a whitespace-only one.  Interactive
    /// only: `--headless` returns through the `reply` tool chat withholds, and
    /// `--system` sets a persona chat obliterates.
    #[arg(long = "chat", conflicts_with_all = ["headless", "system_files"])]
    pub chat: bool,
}

/// The editing scheme `--edit` selects: one system-prompt section, since both
/// editing builtins are registered regardless.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum EditScheme {
    /// Literal string-replacement editing: `view-text`/`view-text-around`/`edit-replace`.
    Replace,
    /// Witnessed line-hash editing: `view-hash`/`view-hash-around`/`edit-hash`.
    Hash,
}

/// An out-of-band action that runs and exits instead of starting a session.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Sign in with `ChatGPT`, adding the account as a provider that runs off
    /// the plan subscription; several can be signed in at once and `/model`
    /// switches between them.  `--device-auth` prints a URL and code instead
    /// of opening a browser.  A live session's `/login` drives the same flow —
    /// this is for the keyless machine that cannot start one.
    Login {
        #[arg(long = "device-auth")]
        device_auth: bool,
    },
    /// Remove a stored `ChatGPT` login.  With no account named it removes the
    /// sole login when exactly one is signed in, and otherwise asks which.
    Logout {
        /// The account to log out of — its login email or account id.
        account: Option<String>,
        /// Log out of every signed-in account.
        #[arg(long, conflicts_with = "account")]
        all: bool,
    },
    /// List the signed-in `ChatGPT` accounts.
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

