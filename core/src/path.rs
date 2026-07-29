//! Path resolution for grant matching.
//!
//! Four stages, one sibling module each: sigil expansion of `~`/`xdg:` at
//! the head (`sigil`), cwd-anchoring and `.`/`..` folding (`lex`),
//! `realpath` with an ancestor-walk fallback (`canon`), and alias-aware
//! containment (`lex::path_within`).
//!
//! Stage 2 mints a [`ResolvedPath`]; the grant side mints a
//! [`NormalizedPrefix`] through the same folding kernel, so an access-side
//! path and a grant-side prefix compare like-for-like.

pub mod basedir;
pub mod canon;
pub mod config;
pub mod git;
pub mod lex;
pub mod prefix_set;
pub mod ral_path;
pub mod render;
pub mod resolved;
pub mod resolver;
pub mod sigil;
pub mod tilde;
pub mod which;

pub use tilde::{
    abbreviate_home, home, home_from_env, home_from_env_or_dot, user_name, user_name_from_env,
};

pub use git::{discover_git_dir, find_git_entry};
pub use lex::{
    PathShape, basename, exists, is_absolute, is_dir, parent_or_cwd, path_aliases, path_within,
    path_within_str, proper_ancestors, resolve_path, resolve_relative_to_script, resolve_str,
    shape,
};
pub use prefix_set::{PrefixSet, covers, meet_prefixes};
#[cfg(target_os = "macos")]
pub(crate) use render::rendered_ancestors;
pub use render::{Rendered, render_paths};
pub use resolved::{Namespace, NormalizedPrefix, ResolvedPath};
pub use resolver::Resolver;
pub use which::{
    commands_on_path, file_exists_on_path, forget_located_commands, locate, resolve_in_path,
};

/// Process working directory, for callers with no shell to ask; shells go
/// through `Shell::cwd`, which honours a `within` override or a prior `cd`.
#[allow(clippy::disallowed_methods)]
pub fn process_cwd() -> Option<std::path::PathBuf> {
    std::env::current_dir().ok()
}

/// `/proc/self/fd/<raw>` as a `PathBuf`.  `sandbox::reexec` uses it to pin
/// the running ral binary across the `execve` into the sandboxed child.
#[cfg(target_os = "linux")]
pub fn proc_fd_path(raw: std::os::fd::RawFd) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/proc/self/fd/{raw}"))
}

/// Placeholder absolute path for fixtures that need a `cwd: &Path`
/// (`sigil::FreezeCtx`) but no particular directory.
#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "lexical Path::new for a test placeholder — no I/O behind it; the lint here guards path-construction discipline, and this shared fixture is its one sanctioned test door"
)]
pub(crate) fn test_cwd() -> &'static std::path::Path {
    std::path::Path::new("/")
}
