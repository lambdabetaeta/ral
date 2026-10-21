//! Path resolution for grant matching.
//!
//! Five stages: sigil expansion of `~`/`xdg:` at the head (`sigil`),
//! cwd-anchoring, and the `.`/`..` fold a logical consumer asks for (`lex`),
//! `realpath` or the walk's lenient form (`canon`), alias-aware containment
//! (`identity::path_within`), and — only where a name is about to be *written
//! through* — symlink-free location of the object itself (`walk`), so the
//! guard judges what the kernel will touch.  Beside the stages, `tree`
//! enumerates a directory tree, judging each entry by its own path.
//!
//! Stage 2 mints a [`LexicalPath`], which the disk reads unfolded; the grant
//! side mints a [`FrozenPath`] from a folded spelling, and the two meet on
//! their real forms.
//!
//! `identity::path_within` and its string twin are `pub(super)` here, so the
//! containment kernel does not leave this module: a prefix carries two
//! forms, and *which* one a question is asked of is settled here, not by a
//! caller — both fs (`FrozenPath::contains`) and exec (`RealPath::within`,
//! over a prefix's frozen `real` form) judge the object.

pub(crate) mod basedir;
pub(crate) mod canon;
pub mod complete;
pub(crate) mod device;
pub(crate) mod forms;
pub mod git;
pub(crate) mod identity;
pub mod lex;
mod namespace;
pub(crate) mod render;
pub(crate) mod resolver;
mod show;
pub mod sigil;
mod slug;
pub mod tilde;
pub mod tree;
pub(crate) mod walk;
pub(crate) mod which;

pub(crate) use basedir::resolve_xdg_in;
pub use basedir::{XdgKind, app_dir, app_dir_or_temp};
pub use show::shown;
pub use slug::project_slug;
pub use tilde::abbreviate_home;

pub use forms::{CwdPath, FrozenPath, LexicalPath, NotACwd};
pub use git::work_tree_chain;
pub use identity::{Allow, Deny, MatchedBy, Polarity};
pub(crate) use identity::{Depth, Subject};
pub(crate) use lex::{
    GrantRefusal, SubpathRefusal, classify_subpath, grant_eligibility, proper_ancestors,
};
pub use lex::{
    NotUtf8, PathShape, basename, exists, extension, is_absolute, is_dir, is_stream_or_drive,
    join_slash, shape, strip_appended_suffix, utf8, with_appended_suffix,
};
pub use namespace::{EnginePath, GuestPath};
#[cfg(unix)]
pub(crate) use render::render_real;
#[cfg(target_os = "macos")]
pub(crate) use render::rendered_ancestors;
#[cfg(unix)]
pub(crate) use render::{Object, Rendered, render_objects, render_paths, rendered_pins};
pub use resolver::Resolver;
pub(crate) use walk::open_discard;
pub use walk::{
    Dir, DirEntry, EntryName, Excluded, Hold, Kind, LeafMode, Located, NotAnEntryName, ObjectId,
    RealPath, RootName, Step, Trace, Walked, walk, walk_dir, walk_lenient,
};
#[cfg(windows)]
pub(crate) use which::is_batch_image;
pub(crate) use which::is_executable_file;
#[cfg(target_os = "linux")]
pub(crate) use which::locate_in;
pub(crate) use which::{PathSearch, path_dirs_absolute, path_entries, search};
pub(crate) use which::{WINDOWS_EXEC_EXTENSIONS, command_name_key, names_a_path, names_a_path_for};
pub use which::{forget_located_commands, locate, path_dirs, ral_path_entries};

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

    /// Whether `c` separates path components: `/` under both rules, `\`
    /// under Windows alone; under Posix a backslash is a filename byte.
    pub(crate) fn is_separator(self, c: char) -> bool {
        c == '/' || (self == Self::Windows && c == '\\')
    }
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

/// A fixture path written POSIX-style, `/a/b`, as an absolute path on this
/// host: on Windows a driveless `/a/b` is not absolute, and a resolver anchors
/// it to the cwd's drive, so it gains a drive and the host's separator.  The
/// root `/` is the universal prefix on every host, as
/// [`FrozenPath::root`] is, and stays as written.
#[cfg(test)]
pub(crate) fn test_absolute(posix: &str) -> String {
    debug_assert!(posix.starts_with('/'), "{posix:?} is not POSIX-absolute");
    match PathRules::HOST {
        PathRules::Windows if posix != "/" => format!("C:{}", posix.replace('/', r"\")),
        _ => posix.to_string(),
    }
}
