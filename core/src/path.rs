//! Path resolution for grant matching.
//!
//! Five stages: sigil expansion of `~`/`xdg:` at the head (`sigil`),
//! cwd-anchoring and `.`/`..` folding (`lex`), `realpath` with an
//! ancestor-walk fallback (`canon`), alias-aware containment
//! (`lex::path_within`), and — only where a name is about to be *written
//! through* — symlink-free location of the object itself (`walk`), so the
//! gate judges what the kernel will touch.
//!
//! Stage 2 mints a [`ResolvedPath`]; the grant side mints a
//! [`NormalizedPrefix`] through the same folding kernel, so an access-side
//! path and a grant-side prefix compare like-for-like.
//!
//! `lex::path_within` and its string twin are `pub(super)` here, so the
//! containment kernel does not leave this module: a prefix carries two
//! forms, and *which* one an authority is judged on is that authority's
//! rule, not a caller's — fs on the object (`prefix_set::covers`), exec on
//! the name (`NormalizedPrefix::grant_depth`, `veto_depth`).  See
//! `docs/ral-wiki/invariants/fs-judges-objects-exec-judges-names.md`.

pub mod basedir;
pub(crate) mod canon;
pub mod config;
pub mod git;
pub mod lex;
pub(crate) mod prefix_set;
pub mod ral_path;
pub(crate) mod render;
pub(crate) mod resolved;
pub(crate) mod resolver;
pub mod sigil;
pub mod tilde;
pub(crate) mod walk;
pub(crate) mod which;

pub use tilde::{abbreviate_home, home, user_name};

pub use git::find_git_entry;
// `crate::path::discover_git_dir` is how `sigil` and its rustdoc name it, so
// the re-export outlives the item's own narrowing.
pub(crate) use git::discover_git_dir;
pub(crate) use lex::proper_ancestors;
pub use lex::{
    PathShape, basename, exists, is_absolute, is_dir, resolve_path, resolve_relative_to_script,
    resolve_str, shape,
};
pub(crate) use prefix_set::{PrefixSet, covers, meet_prefixes};
#[cfg(target_os = "macos")]
pub(crate) use render::rendered_ancestors;
pub(crate) use render::rendered_pins;
pub(crate) use render::{Rendered, render_paths};
pub use resolved::{Namespace, NormalizedPrefix, ResolvedPath};
pub use resolver::Resolver;
pub use walk::Located;
pub(crate) use which::is_executable_file;
pub(crate) use which::{PathSearch, search};
pub use which::{SearchCwd, forget_located_commands, locate, resolve_in_path};

/// Process working directory, for callers with no shell to ask; shells go
/// through `Shell::cwd`, which honours a `within` override or a prior `cd`.
#[allow(clippy::disallowed_methods)]
pub fn process_cwd() -> Option<std::path::PathBuf> {
    std::env::current_dir().ok()
}

/// `/proc/self/fd/<raw>` as a `PathBuf`.  `sandbox::reexec` uses it to pin
/// the running ral binary across the `execve` into the sandboxed child.
#[cfg(target_os = "linux")]
pub(crate) fn proc_fd_path(raw: std::os::fd::RawFd) -> std::path::PathBuf {
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
