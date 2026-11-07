//! The lexer: what a path's spelling says, read with no filesystem access,
//! under either platform's rules from any host.  `head` parses a head once,
//! and every judgment of it projects from that parse; `eligibility` says
//! which platforms a grant's raw payload is live on; `anchoring` reads a
//! relative spelling against a cwd; `fold` folds `.` and `..`; `join` joins a
//! subpath; `name` reads a path's names; `command` reads a command name.
//!
//! Containment and name identity live in [`super::identity`]; device names in
//! [`super::device`].

mod anchoring;
mod command;
mod eligibility;
mod fold;
mod head;
mod join;
mod name;

pub(crate) use anchoring::{anchor, anchor_on, resolve_path};
#[cfg(any(windows, test))]
pub(crate) use command::is_batch_image;
pub(crate) use command::{WINDOWS_EXEC_EXTENSIONS, names_a_path, names_a_path_for};
pub(crate) use eligibility::{GrantRefusal, SubpathRefusal, classify_subpath, grant_eligibility};
pub(crate) use fold::{fold_dots, fold_dots_under};
pub use head::is_absolute;
#[cfg(any(windows, test))]
pub(crate) use head::is_volume_guid;
pub(in crate::path) use head::{HeadKind, parse_bytes};
pub(crate) use head::{
    absolute_under, has_drive, is_bare_root, is_unparsed_head, is_working_directory,
    windows_components, windows_head,
};
#[cfg(test)]
pub(crate) use head::{is_network, is_verbatim_drive};
pub(crate) use join::{Rerooting, join_relative, join_under, relative_suffix};
pub use join::{join_slash, strip_appended_suffix, with_appended_suffix};
pub use name::{
    NotUtf8, basename, descendant, extension, file_name, key_within, relative_key, spelled_parent,
    utf8,
};
pub(crate) use name::{names_a_dir, parent_or_cwd};

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
