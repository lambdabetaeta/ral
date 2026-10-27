//! Paths: a name is never authority, one walk resolves it, and the judgment
//! consumes the transcript.
//!
//! `sigil` and `lex` are lexical: a `~` or `xdg:` head expanded, a relative
//! spelling anchored on the cwd as a [`LexicalPath`], which the disk reads
//! unfolded, and the `.`/`..` fold a [`FrozenPath`] shows.  `walk` is the one
//! resolver: a spelling walked from the root through directory handles to the
//! object it names, with the [`Trace`] of every object opened, which the grant
//! judges and the handle then acts on.  `identity` holds the containment and
//! matching kernel over positions and traces; `canon` and `render` give the
//! names a kernel sandbox rule may mention; `tree` enumerates a directory
//! tree, judging each entry as it goes.
//!
//! `walk` imports `Verdict` (`capability/fs.rs`) and `InstallToken`
//! (`sandbox.rs`): two inverted edges, each a token whose field only its own
//! module can make, so neither constructor may be widened to straighten them.

pub(crate) mod basedir;
pub(crate) mod canon;
pub mod complete;
pub(crate) mod device;
pub(crate) mod forms;
pub mod git;
pub(crate) mod identity;
pub mod lex;
mod namespace;
#[cfg(target_os = "linux")]
mod procfs;
#[cfg(unix)]
pub(crate) mod render;
pub(crate) mod resolver;
mod rules;
mod show;
pub mod sigil;
mod site;
mod slug;
pub mod tilde;
pub mod tree;
pub(crate) mod walk;
pub(crate) mod which;

pub(crate) use basedir::resolve_xdg_in;
pub use basedir::{XdgKind, app_dir};
pub use show::shown;
pub use slug::project_slug;
pub use tilde::abbreviate_home;

pub use forms::{CwdPath, FrozenPath, LexicalPath, NotACwd};
pub use git::work_tree_chain;
pub use identity::MatchedBy;
pub(crate) use identity::{Depth, Subject};
#[cfg(target_os = "macos")]
pub(crate) use lex::proper_ancestors;
pub(crate) use lex::{GrantRefusal, SubpathRefusal, classify_subpath, grant_eligibility};
pub use lex::{
    NotUtf8, basename, descendant, extension, is_absolute, join_slash, relative_key,
    spelled_parent, strip_appended_suffix, utf8, with_appended_suffix,
};
pub use namespace::{EnginePath, GuestPath};
#[cfg(unix)]
pub(crate) use render::render_real;
#[cfg(target_os = "macos")]
pub(crate) use render::rendered_ancestors;
#[cfg(unix)]
pub(crate) use render::{Object, Rendered, pinned_dirs, render_excluded, render_objects};
pub use resolver::{Resolver, spelled};
pub(crate) use walk::is_loop;
pub use walk::{
    Authority, Dir, DirEntry, EntryName, Excluded, Hold, Installing, Judged, Kind, LeafMode,
    Listing, Lists, Located, NotAnEntryName, ObjectId, Op, Peeks, Reading, Reads, RealPath,
    RootName, Stat, Step, Trace, Unjudged, Visibility, Walked, Writing, exists, is_dir, kind_of,
    walk, walk_dir, walk_lenient,
};
pub(crate) use walk::{StagedWrite, Streamed, Streaming, open_discard};
#[cfg(windows)]
pub(crate) use which::is_batch_image;
pub(crate) use which::is_executable_file;
#[cfg(target_os = "linux")]
pub(crate) use which::locate_in;
pub(crate) use which::{PathSearch, search};
pub(crate) use which::{WINDOWS_EXEC_EXTENSIONS, command_name_key, names_a_path, names_a_path_for};
pub use which::{forget_located_commands, locate, path_dirs, path_entries, ral_path_entries};

#[cfg(target_os = "linux")]
pub(crate) use procfs::proc_fd_path;
pub(crate) use rules::PathRules;
