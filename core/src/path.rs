//! Paths: a name is never authority, one walk resolves it, and the judgment
//! consumes the transcript.
//!
//! Lexical, reading no disk:
//! - `rules`: which platform's rules a question is asked under.
//! - `lex`: the lexer, a head parsed once and what projects from it: a
//!   relative spelling anchored on a cwd, the `.`/`..` fold, joins, a path's
//!   names, a grant payload's eligibility, a command name.
//! - `sigil` and `tilde`: a `~` or `xdg:` head, expanded at run time;
//!   `basedir`, the XDG bases an `xdg:` head reads.
//! - `device`: the discard sink, and the DOS names no file may take.
//! - `forms`: the normal forms the grant pipeline trusts ([`CwdPath`],
//!   [`LexicalPath`], [`FrozenPath`]); `namespace`, whose machine a path
//!   names ([`GuestPath`], [`EnginePath`]).
//! - `resolver`: the lexical stages composed, the sole maker of a
//!   [`LexicalPath`] ([`Resolver`]).
//! - `show`: a path as ral shows it ([`shown`]); `slug`, a cwd as one
//!   directory name; `complete`, the path half of completing a word.
//!
//! Physical:
//! - `handle`: the one resolver, a spelling walked from the root through
//!   directory handles to its object ([`walk()`]), with the [`Trace`] of
//!   every object opened, which the grant judges and the handle then acts on.
//! - `which`: `PATH` search, each candidate walked; `git`, git-directory
//!   discovery; `shebang`, an executable's `#!` line; `procfs`, a pinned
//!   descriptor's `/proc` name.
//! - `identity`: the containment and matching kernel over positions and
//!   traces, and a command name's key; `canon`, the names one object answers
//!   to; `render`, the names a kernel sandbox rule may mention.
//! - `tree`: a directory tree enumerated, each entry judged as it goes.
//! - `site`: the typed sites in this module that lift a ban.
//!
//! One path per item: an item used outside `path/` is `crate::path::Item`,
//! re-exported here at the reach its users need from a private module.  The
//! public modules are namespaces, whose items read only through their name
//! and are re-exported nowhere: `tree` (`tree::Walk`), `git`
//! (`git::Pointer`) and `complete` (`complete::TildeReading`).
//!
//! `handle` imports `Verdict` (`authority/policy/fs.rs`) and `InstallToken`
//! (`sandbox.rs`): two inverted edges, each a token whose field only its own
//! module can make, so neither constructor may be widened to straighten them.

mod basedir;
mod canon;
pub mod complete;
mod device;
mod forms;
pub mod git;
mod handle;
mod identity;
mod lex;
mod namespace;
#[cfg(target_os = "linux")]
mod procfs;
#[cfg(unix)]
mod render;
mod resolver;
mod rules;
#[cfg(unix)]
mod shebang;
mod show;
mod sigil;
mod site;
mod slug;
mod tilde;
pub mod tree;
mod which;

pub(crate) use basedir::resolve_xdg_in;
pub use basedir::{XdgKind, app_dir};
#[cfg(all(test, target_os = "macos"))]
pub(crate) use canon::twins;
pub use forms::{CwdPath, FrozenPath, LexicalPath, NotACwd};
#[cfg(test)]
pub(crate) use handle::directory_refusal_for_test;
#[cfg(any(test, feature = "test-util"))]
pub(crate) use handle::steps_taken;
pub(crate) use handle::{
    Access, StagedWrite, Streamed, Streaming, dangling, directory_refused, failure_trace, is_loop,
    open_discard, sync_dir, sync_dir_chain, unsynced, walk_dir_made, walk_dir_made_synced,
};
pub use handle::{
    Authority, Dir, DirEntry, EntryName, Excluded, Handle, Hold, Installing, Judged, Kind,
    LeafMode, Listing, Lists, Located, NotAnEntryName, ObjectId, Op, Peeks, Reading, Reads,
    RealPath, RootName, ScopedDir, Stat, Step, Trace, Unjudged, Visibility, Walked, Writing,
    exists, kind_of, walk, walk_dir, walk_lenient,
};
#[cfg(unix)]
pub(crate) use handle::{Vetted, walk_with};
pub use identity::MatchedBy;
#[cfg(windows)]
pub(crate) use identity::windows_collision_key;
pub(crate) use identity::{Depth, Subject, command_name_key};
#[cfg(test)]
pub(crate) use lex::fold_dots;
pub(crate) use lex::{
    GrantRefusal, SubpathRefusal, classify_subpath, grant_eligibility, has_drive, names_a_path,
    names_a_path_for, relative_suffix, windows_components,
};
pub use lex::{
    NotUtf8, basename, descendant, extension, file_name, is_absolute, join_slash, key_within,
    relative_key, spelled_parent, strip_appended_suffix, utf8, with_appended_suffix,
};
#[cfg(windows)]
pub(crate) use lex::{WINDOWS_EXEC_EXTENSIONS, is_batch_image, is_volume_guid};
pub use namespace::{EnginePath, GuestPath};
#[cfg(target_os = "linux")]
pub(crate) use procfs::proc_fd_path;
#[cfg(all(test, target_os = "macos"))]
pub(crate) use render::proper_ancestors;
#[cfg(unix)]
pub(crate) use render::render_real;
#[cfg(target_os = "macos")]
pub(crate) use render::rendered_ancestors;
#[cfg(unix)]
pub(crate) use render::{Object, Rendered, pinned_dirs, render_excluded, render_objects};
pub use resolver::{Resolver, spelled};
pub(crate) use rules::PathRules;
pub use show::shown;
pub(crate) use sigil::SigilHead;
#[cfg(test)]
pub(crate) use sigil::expand_path_prefix;
pub use slug::project_slug;
pub use tilde::{TildePath, abbreviate_home};
#[cfg(target_os = "linux")]
pub(crate) use which::locate_in;
#[cfg(windows)]
pub(crate) use which::with_exec_extensions;
pub(crate) use which::{Candidate, search};
pub use which::{forget_located_commands, locate, path_dirs, path_entries, ral_path_entries};
