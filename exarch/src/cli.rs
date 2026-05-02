//! Command-line surface.
//!
//! Parses argv via clap and resolves `-p/-f` into the optional initial
//! prompt.  The system-prompt assembly lives in `prompt::assemble`.

use crate::api::ProviderKind;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(about = "Exarch — a delegate driving ral under a grant", long_about = None)]
pub struct Cli {
    /// All flags are long-form only — short-letter aliases collide
    /// with each other in unhelpful ways (e.g. `-p` provider vs
    /// prompt) and there are few enough flags that long names are
    /// fine.
    #[arg(long, value_enum, default_value_t = ProviderKind::Anthropic)]
    pub provider: ProviderKind,
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
    /// Single TOML file lattice-joined with the base *before* any
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
}

/// Resolve `-p/-f` into an optional initial prompt.
pub fn load_seed(prompt: Option<String>, file: Option<std::path::PathBuf>) -> Result<Option<String>, String> {
    Ok(match (prompt, file) {
        (Some(p), _) => Some(p),
        (_, Some(path)) => {
            Some(std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?)
        }
        _ => None,
    })
}
