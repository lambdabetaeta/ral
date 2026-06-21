//! Command-line surface.
//!
//! Parses argv via clap and resolves `-p/-f` into the optional initial
//! prompt.  The system-prompt assembly lives in `prompt::assemble`.

use crate::headless::OutputFormat;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(about = "Exarch — a delegate driving ral under a grant", long_about = None)]
pub struct Cli {
    /// A subcommand runs an out-of-band action and exits.  Absent — the
    /// normal case — exarch starts a session governed by the flags below.
    #[command(subcommand)]
    pub command: Option<Command>,
    /// All flags are long-form only — short-letter aliases collide
    /// with each other in unhelpful ways and there are few enough
    /// flags that long names are fine.
    ///
    /// Optional initial-model override for headless/scripted use. Its
    /// provider is resolved as the available provider whose model list
    /// contains the name, or — for a `vendor/model` slug — OpenRouter,
    /// or the sole available provider; unknown names error clearly.
    /// Unset, the selection comes from the saved state or the first available
    /// provider's default model. The interactive `/model` picker is the
    /// normal way to choose a model.
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long, conflicts_with = "file")]
    pub prompt: Option<String>,
    #[arg(long)]
    pub file: Option<std::path::PathBuf>,
    #[arg(long = "system", value_name = "FILE")]
    pub system_files: Vec<std::path::PathBuf>,
    /// Session ceiling.  Five bake-ins, ordered from most to least
    /// authority: `dangerous` (no attenuation; expects an outer
    /// trust boundary like a Docker container), `reasonable`
    /// (default; everyday tooling + standard binary dirs),
    /// `read-only` (reasonable's reads/exec but writes only to
    /// scratch — for review/audit), `minimal` (coreutils + cwd +
    /// scratch + net; small base for additive `--extend-base`
    /// composition), `confined` (build jail after BrianSwift's
    /// confined.sb: tight reads/writes, no network, exec by subpath
    /// only).  Bases are bake-ins; there is no directory convention
    /// for adding more.  To widen the ceiling for a nonstandard
    /// tool, use `--extend-base`; to start permissive, use
    /// `--base dangerous --restrict <FILE>` (root ⊓ file = file).
    #[arg(long = "base", value_name = "NAME", default_value = "reasonable")]
    pub base: String,
    /// Single ral file lattice-joined with the base *before* any
    /// attenuation, widening the ceiling.  Use to add allowances for
    /// nonstandard tools (extra exec entries, fs prefixes) without
    /// editing a bake-in.  Trust boundary: this widens, so source it
    /// from your own config — never auto-loaded from cwd.
    #[arg(long = "extend-base", value_name = "FILE")]
    pub extend_base: Option<std::path::PathBuf>,
    /// Attenuation file(s) meet-composed with the (possibly extended)
    /// base.  Repeatable; order doesn't matter (meet is commutative).
    /// Each file's resolved path is added to the fs deny list, so the
    /// agent cannot modify any file influencing its own permissions.
    #[arg(long = "restrict", value_name = "FILE")]
    pub restrict: Vec<std::path::PathBuf>,
    /// Per-request visible-output ceiling (`max_tokens` in the API
    /// call).  Unset = use the per-model default (32k for Opus, 16k
    /// for Sonnet, 8k for Haiku and unknown models).  The old hard
    /// 4k constant truncated Opus turns mid-stream and reported them
    /// as silent stops; raise this when emitting large files or
    /// running deep reasoning.
    #[arg(long = "max-tokens", value_name = "N")]
    pub max_tokens: Option<u32>,
    /// Run one seed turn non-interactively: the assistant's reply
    /// streams to stdout, every other event (tool calls, results,
    /// errors) condenses to one line on stderr, and the process exits
    /// when the turn finishes.  Requires `--prompt` or
    /// `--file`; no alt-screen, no REPL.  Pipe-friendly.
    #[arg(long)]
    pub headless: bool,
    /// Headless stdout format.  `text` (default) streams the root
    /// agent's assistant text live; `json` holds it back and emits one
    /// result object (final message, stop reason, turns, duration,
    /// token usage + cost) when the run ends.  Only meaningful with
    /// `--headless`, which clap enforces when this flag is given.
    #[arg(long = "output-format", value_enum, default_value_t = OutputFormat::Text, requires = "headless")]
    pub output_format: OutputFormat,
    /// In headless mode, gate turn completion on a single self-confirming
    /// nudge: a turn that never used a tool is nudged to engage, and a turn
    /// that did is nudged to verify its output against the task before
    /// finishing. Each nudge fires at most once per turn. Off by default so
    /// plain question-answering (the common headless use) is unaffected.
    #[arg(long = "expect-action", requires = "headless")]
    pub expect_action: bool,

    /// Authorise the agent to schedule its own wakeups (the `schedule`,
    /// `schedules`, `unschedule` tools).  Off by default: an agent that can
    /// wake itself indefinitely holds real authority, so opt in explicitly.
    #[arg(long = "allow-schedule")]
    pub allow_schedule: bool,

    /// Edit the TUI prompt in vi mode (the shared `textarea-vim` state
    /// machine, the same one the ral worksheet uses).  Off by default —
    /// the prompt edits emacs-style.  With vi mode on the prompt opens in
    /// insert mode; Esc drops to normal mode for motions and operators.
    #[arg(long = "vi")]
    pub vi: bool,
}

/// An out-of-band action that runs and exits instead of starting a session.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Sign in with ChatGPT, adding the account as a provider that runs off
    /// the plan subscription.  Several accounts can be signed in at once; the
    /// `/model` picker switches between them.  Signing in again with the same
    /// account refreshes its tokens.  Opens a browser by default;
    /// `--device-auth` prints a URL and code instead, for a machine with no
    /// local browser.
    Login {
        #[arg(long = "device-auth")]
        device_auth: bool,
    },
    /// Remove a stored ChatGPT login.  Names the account to remove (by its
    /// email or account id); with no account it removes the sole login when
    /// exactly one is signed in, and otherwise asks which.  `--all` removes
    /// every account.
    Logout {
        /// The account to log out of — its login email or account id.
        account: Option<String>,
        /// Log out of every signed-in account.
        #[arg(long, conflicts_with = "account")]
        all: bool,
    },
    /// List the signed-in ChatGPT accounts.
    Accounts,
}

/// Resolve `-p/-f` into an optional initial prompt.  A blank seed (empty
/// or whitespace-only) collapses to `None` here, so the frontends do not
/// each re-filter: the headless frontend rejects a missing seed, the TUI
/// simply opens to a prompt.
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
        (_, Some(path)) => {
            Some(std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?)
        }
        _ => None,
    };
    Ok(seed.filter(|s| !s.trim().is_empty()))
}
