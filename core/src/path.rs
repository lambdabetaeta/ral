//! Path resolution for grant matching.
//!
//! Every grant-touching path obeys one operational rule, in this
//! order, and each premise has its own sibling file:
//!
//! ```text
//!   expand dyn p   ⇓  q     stage 1: ~ and xdg: at the head        (sigil)
//!   lex   dyn q    ⇓  r     stage 2: cwd-anchor + ./.. normalise   (lex → ResolvedPath)
//!   canon r        ⇓  c     stage 3: realpath, ancestor-walk fallback (canon)
//!   match a c P             stage 4: alias-aware containment       (lex::path_within)
//! ```
//!
//! Stage 2 mints a [`ResolvedPath`]; the grant side mints a
//! [`NormalizedPrefix`] under the same kernel.  Both are single-
//! constructor newtypes (see [`resolved`]), so canonicalisation and
//! grant matching can only see a normalised path.
//!
//! [`tilde`] holds the syntactic shape consumed by stage 1 (and
//! by the lexer) as well as [`home`] / [`home_from_env`] /
//! [`home_from_env_or_dot`] for `$HOME` and [`user_name`] /
//! [`user_name_from_env`] for `$USER` resolution; [`which`] is a
//! sibling for `PATH` search.
//!
//! Most call sites want the most-used names without reaching
//! into a child module — those are re-exported below.  The full
//! API lives in the children, named by stage.

pub mod basedir;
pub mod canon;
pub mod config;
pub mod git;
pub mod lex;
pub mod prefix_set;
pub mod ral_path;
pub mod resolved;
pub mod resolver;
pub mod sigil;
pub mod tilde;
pub mod which;

pub use tilde::{home, home_from_env, home_from_env_or_dot, user_name, user_name_from_env};

pub use canon::match_variants_list;
pub use git::{discover_git_dir, find_git_entry};
pub use lex::{
    basename, exists, is_absolute, parent_or_cwd, path_aliases, path_within, path_within_str,
    proper_ancestors, resolve_path, resolve_relative_to_script, resolve_str,
};
pub use prefix_set::PrefixSet;
pub use resolved::{NormalizedPrefix, ResolvedPath};
pub use resolver::{CanonMode, Resolver};
pub use which::{commands_on_path, file_exists_on_path, locate, resolve_in_path};

/// Process working directory.  The one syscall behind the lint —
/// `Shell::cwd` is the canonical accessor for shells; this helper is
/// for the few shell-less callers (path resolver fallback, sandbox
/// host snapshot, [`Resolver::resolve`] when no logical cwd is bound).
#[allow(clippy::disallowed_methods)]
pub fn process_cwd() -> Option<std::path::PathBuf> {
    std::env::current_dir().ok()
}

/// `/proc/self/fd/<raw>` as a `PathBuf`.  Linux-only: the magic
/// procfs entry that names an open file descriptor by path, which
/// the sandbox re-exec uses to pin the ral binary across the
/// `execve` into the child.
///
/// The single caller is in `sandbox::reexec`; living here keeps the
/// procfs literal in one place and out of the workspace-wide
/// `disallowed_methods` lint for `PathBuf::from`.
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)]
pub fn proc_fd_path(raw: std::os::fd::RawFd) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/proc/self/fd/{raw}"))
}
