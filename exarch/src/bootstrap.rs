//! Sandbox dispatch, shell construction, scratch directory.
//!
//! One-shot bootstrap pieces that live for the lifetime of the process
//! (or, for [`Scratch`], for the lifetime of one root session).
//! Nothing here participates in the per-turn loop.  Per-session disk
//! state (the canonical event log) lives in [`crate::event::AgentLog`].

use crate::{agent_builtins, cancel, shell_eval};
use ral_core::io::TerminalState;
use ral_core::{Shell, diagnostic};
use std::fs;
use std::io;
use std::path::PathBuf;

/// Probe the terminal in the same mode `boot_shell` and `Agent::fork`
/// both want — honours `RAL_INTERACTIVE_MODE` if set.
pub fn probe_terminal() -> TerminalState {
    let (_mode, terminal, _warn) = TerminalState::probe_from_env();
    terminal
}

/// Build a shell ready for an exarch session.
///
/// This is the one constructor that may boot a session shell: it resets
/// ral's signal-escalation ladder before embedded library evaluation,
/// installs ral's handlers, then immediately layers exarch's cancel chain
/// over them.  Callers should seed per-session variables, not repeat
/// signal ceremony.
pub fn boot_shell() -> Shell {
    ral_core::process::clear();
    ral_core::process::install_handlers();
    cancel::install();

    let terminal = probe_terminal();
    diagnostic::set_terminal(&terminal);
    let mut shell = ral_core::driver::boot_shell(terminal, &shell_eval::PRELUDE);
    agent_builtins::install_on(&mut shell);
    agent_builtins::install_agent_library(&mut shell)
        .unwrap_or_else(|e| panic!("exarch: embedded agent library failed to load: {e:?}"));
    ral_core::builtins::misc::register_library_docs(agent_builtins::agent_library_docs());
    seed_no_color(&mut shell);
    shell.set_exit_hints(ral_core::exit_hints::ExitHints::from_text(include_str!(
        "../../data/exit-hints.txt"
    )));
    shell
}

/// Suppress ANSI colour in spawned commands at the source.  Every tool
/// call runs with stdout/stderr captured on a pipe — never a TTY — so
/// conforming tools already emit no colour; these override user config
/// and an inherited `CLICOLOR_FORCE` that *force* it.  That keeps a
/// captured value byte-identical to the text [`crate::digest`] shows
/// the model; its ANSI strip remains the boundary guarantee for tools
/// that ignore the convention.  Env-only, no ral scope binding: the
/// agent has no reason to read these back.
pub(crate) fn seed_no_color(shell: &mut Shell) {
    shell.set_env_var("NO_COLOR", "1");
    shell.set_env_var("CLICOLOR_FORCE", "0");
}

/// Per-session scratch directory exposed to the agent as `$EXARCH_SCRATCH`.
///
/// Caches the agent might want to scribble to (build artefacts, package
/// manager state, anything ephemeral) live here instead of in the user's
/// real cache dirs (`~/.cargo/registry`, `~/.npm`, …) — those are denied
/// for write by `reasonable`, so a direct write there fails loudly.
///
/// To make this transparent to the agent, [`Scratch::install_into`]
/// redirects a small fixed list of legacy tool env vars (`CARGO_HOME`,
/// `npm_config_cache`, `GRADLE_USER_HOME`, `GOPATH`, `GOMODCACHE`,
/// `RUSTUP_HOME`) to subdirs of `$EXARCH_SCRATCH`.  Modern tools that
/// respect `$XDG_CACHE_HOME` need no per-tool redirection — the
/// `xdg:cache` write admit in reasonable handles them.
///
/// Intentionally left on disk when the session ends; scratch lives
/// under OS-managed temp space and can be swept by the platform.
pub struct Scratch {
    dir: PathBuf,
}

/// Legacy build-tool home env vars that pre-date or ignore XDG.  Each
/// gets a dedicated subdir under `$EXARCH_SCRATCH` so the toolchains
/// can do their own bookkeeping without colliding.  This list is
/// deliberately stable and short — modern tools added since ~2018
/// (uv, pnpm, bun, mise, hatch, ruff, …) respect `$XDG_CACHE_HOME`
/// and don't need an entry here.
const LEGACY_TOOL_HOMES: &[(&str, &str)] = &[
    ("CARGO_HOME", "cargo"),
    ("npm_config_cache", "npm-cache"),
    ("GRADLE_USER_HOME", "gradle"),
    ("GOPATH", "go"),
    ("GOMODCACHE", "go/pkg/mod"),
    ("RUSTUP_HOME", "rustup"),
];

impl Scratch {
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:scratch-bootstrap] disposable scratch-dir setup; not turn-time data I/O"
    )]
    pub fn new() -> io::Result<Self> {
        let dir = std::env::temp_dir().join(format!("exarch-scratch-{}", std::process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.dir
    }

    /// A uniquely-named scratch for in-crate tests: [`Self::new`]'s dir is
    /// keyed by pid alone, so two tests constructing scratches concurrently
    /// would contend on one path — the `tag` keeps each test's dir its own.
    /// Compiled only under test; a live run always owns the pid-keyed dir.
    #[cfg(test)]
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:test] test fs scaffolding — a per-test scratch dir"
    )]
    pub(crate) fn for_test(tag: &str) -> io::Result<Self> {
        let dir =
            std::env::temp_dir().join(format!("exarch-scratch-test-{}-{tag}", std::process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// Seed `$EXARCH_SCRATCH` and the legacy-tool env vars into
    /// `shell` — both the env-var map (so child processes inherit
    /// them) and the ral-side bindings (so the same names resolve
    /// inside ral source).  Always overrides: a user pre-set
    /// `CARGO_HOME` pointing into `~/.cargo` would land outside
    /// reasonable's write set and fail mysteriously, so the sandbox
    /// is the trust boundary, not the inherited environment.
    pub fn install_into(&self, shell: &mut Shell) {
        let scratch = self.dir.to_string_lossy().into_owned();
        seed_var(shell, "EXARCH_SCRATCH", &scratch);
        for (var, sub) in LEGACY_TOOL_HOMES {
            let value = format!("{scratch}/{sub}");
            seed_var(shell, var, &value);
        }
    }
}

/// The per-run log directory: `$XDG_STATE_HOME/exarch/<project>/<run>/`.
///
/// `<project>` is the
/// slugified absolute `cwd` (see [`project_slug`]) and `<run>` is
/// `<YYYY-MM-DD-HHMMSS>-<pid>`, unique per launch so successive runs in the
/// same project never overwrite one another.  Holds `stderr.log` and
/// `sessions/<id>/{events.json,transcript.jsonl,user.log}`.  Unlike the disposable
/// [`Scratch`] this is durable state under the user's XDG state home, so
/// it survives an abnormal exit and stays findable without a symlink.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:log-run-dir] per-run log dir under XDG state; infra, not turn-time data I/O"
)]
pub fn log_run_dir(cwd: &str) -> io::Result<PathBuf> {
    let stamp = format!("{}-{}", stamp_from_secs(now_secs()), std::process::id());
    let dir = project_dir(cwd).join(stamp);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The current time in whole unix seconds, or 0 if the clock is before the
/// epoch. The one spelling shared by the run-dir stamp, the model cache's
/// freshness check ([`crate::models`]), and the OAuth token expiry
/// ([`crate::oauth`]).
pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Format a Unix timestamp as `YYYY-MM-DD-HHMMSS` (UTC) via `jiff`.
/// Falls back to the raw seconds string if the timestamp is out of range.
fn stamp_from_secs(secs: u64) -> String {
    jiff::Timestamp::from_second(secs as i64)
        .map_or_else(|_| secs.to_string(), |t| t.strftime("%Y-%m-%d-%H%M%S").to_string())
}

/// The per-project directory `$XDG_STATE_HOME/exarch/<project>/`, where
/// `<project>` is the slugified absolute `cwd` (see [`project_slug`]).
///
/// Both the persisted model selection (`state.json`) and the per-run
/// session logs live under it, so a project's exarch state is one findable
/// directory keyed by where it was launched — never scattered into cwd.
pub fn project_dir(cwd: &str) -> PathBuf {
    xdg_app_dir(ral_core::path::basedir::XdgKind::State).join(project_slug(cwd))
}

/// The exarch directory under an XDG base: `$XDG_<kind>_HOME/exarch/`.
///
/// The
/// one spelling of the app-subdir convention — [`project_dir`] (state home)
/// and the model cache (`models::cache_path`, cache home) both build on it.
pub fn xdg_app_dir(kind: ral_core::path::basedir::XdgKind) -> PathBuf {
    ral_core::path::basedir::resolve_xdg(kind, &ral_core::path::home_from_env()).join("exarch")
}

/// Slugify an absolute path into one directory-name component by joining
/// its components with `-`, a leading `-` standing in for the root:
/// `/Users/x/proj` becomes `-Users-x-proj`.  Built on
/// [`std::path::Path::components`] so it is correct across `/`, `\`, and
/// Windows drive prefixes without a slugifier dependency.
// `Path::components` is the correct cross-platform decomposition for a
// slug; this is not a path-resolution site, so it is exempt from the
// crate::path discipline.
#[allow(clippy::disallowed_methods)]
fn project_slug(cwd: &str) -> String {
    use std::path::Component;
    let mut parts: Vec<String> = Vec::new();
    let mut rooted = false;
    for comp in std::path::Path::new(cwd).components() {
        match comp {
            Component::Prefix(p) => {
                let drive: String = p
                    .as_os_str()
                    .to_string_lossy()
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .collect();
                if !drive.is_empty() {
                    parts.push(drive);
                }
            }
            Component::RootDir => rooted = true,
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::CurDir | Component::ParentDir => {}
        }
    }
    let body = parts.join("-");
    let slug = if rooted { format!("-{body}") } else { body };
    if slug.is_empty() { "_".into() } else { slug }
}

/// Seed `name` into `shell` as both an environment variable (so spawned
/// child processes inherit it) and a ral-side scope binding (so `$name`
/// resolves in ral source).  Shared by [`Scratch::install_into`] and the
/// per-agent `EXARCH_SESSION_DIR` seeding in [`crate::agent`].
pub(crate) fn seed_var(shell: &mut Shell, name: &str, value: &str) {
    shell.set_env_var(name, value);
    shell.set_var(name.into(), ral_core::types::Value::String(value.into()));
}

#[cfg(test)]
mod tests {
    use super::project_slug;

    #[test]
    fn slug_joins_path_components_with_dashes() {
        assert_eq!(project_slug("/Users/x/ral-private"), "-Users-x-ral-private");
        assert_eq!(project_slug("/"), "-");
    }
}
