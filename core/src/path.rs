//! Path resolution for grant matching.
//!
//! Five stages: sigil expansion of `~`/`xdg:` at the head (`sigil`),
//! cwd-anchoring and `.`/`..` folding (`lex`), `realpath` with an
//! ancestor-walk fallback (`canon`), alias-aware containment
//! (`identity::path_within`), and — only where a name is about to be *written
//! through* — symlink-free location of the object itself (`walk`), so the
//! guard judges what the kernel will touch.
//!
//! Stage 2 mints a [`LexicalPath`]; the grant side mints a
//! [`FrozenPath`] through the same folding kernel, so an access-side
//! path and a grant-side prefix compare like-for-like.
//!
//! `identity::path_within` and its string twin are `pub(super)` here, so the
//! containment kernel does not leave this module: a prefix carries two
//! forms, and *which* one a question is asked of is settled here, not by a
//! caller — both fs (`FrozenPath::contains`) and exec (`RealPath::within`,
//! over a prefix's frozen `real` form) judge the object.

pub(crate) mod canon;
pub(crate) mod device;
pub(crate) mod forms;
pub mod git;
pub(crate) mod identity;
pub mod lex;
pub mod ral_path;
mod real;
pub(crate) mod render;
pub(crate) mod resolver;
pub mod sigil;
pub(crate) mod stage;
pub mod tilde;
pub(crate) mod walk;
pub(crate) mod which;

pub use tilde::abbreviate_home;

pub use forms::{FrozenPath, LexicalPath};
pub use git::find_git_entry;
pub(crate) use identity::{Allow, Deny, Polarity};
pub(crate) use lex::proper_ancestors;
pub use lex::{
    PathShape, basename, exists, is_absolute, is_dir, resolve_path, resolve_relative_to_script,
    resolve_str, shape,
};
pub(crate) use real::RealPath;
#[cfg(unix)]
pub(crate) use render::render_real;
#[cfg(target_os = "macos")]
pub(crate) use render::rendered_ancestors;
pub(crate) use render::rendered_pins;
#[cfg(target_os = "linux")]
pub(crate) use render::{Object, render_objects};
pub(crate) use render::{Rendered, render_paths};
pub use resolver::Resolver;
pub use walk::Located;
pub(crate) use which::is_executable_file;
pub(crate) use which::{PathSearch, search};
pub use which::{SearchCwd, forget_located_commands, locate, resolve_in_path};
pub(crate) use which::{WINDOWS_EXEC_EXTENSIONS, command_name_key};

/// Which platform's path rules a question is asked under.
///
/// A parameter rather than a `cfg!` read, so both tables are pinned by tests
/// on every host; the gate is [`Self::HOST`], read at the door.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathRules {
    Posix,
    Windows,
}

impl PathRules {
    pub(crate) const HOST: Self = if cfg!(windows) {
        Self::Windows
    } else {
        Self::Posix
    };
}

/// `/proc/self/fd/<raw>` as a `PathBuf`: how a binary `sandbox::reexec`
/// pinned by fd (ral, bwrap) is exec'd, by the parent from the pin and by
/// bwrap from the slot the pin was lent at.
#[cfg(target_os = "linux")]
pub(crate) fn proc_fd_path(raw: std::os::fd::RawFd) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/proc/self/fd/{raw}"))
}

/// Placeholder absolute path for fixtures that need a `cwd: &Path`
/// (`guard::freeze::FreezeCtx`) but no particular directory.
#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "lexical Path::new for a test placeholder: no I/O behind it; the lint here guards path-construction discipline, and this shared fixture is its one sanctioned test door"
)]
pub(crate) fn test_cwd() -> &'static std::path::Path {
    std::path::Path::new("/")
}
