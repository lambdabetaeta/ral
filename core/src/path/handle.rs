//! Handles: a spelling walked through directory handles to its object, and
//! the transcript a judgment reads.
//!
//! A name is never authority. [`walk`] descends through directory handles,
//! splicing links itself; its [`Trace`] records what it opened. Operations
//! use those same handles, only after the grant stack or sandbox vouched for
//! them through [`Authority`].
//!
//! `names` validates spellings; `trace` holds the evidence; `exclusion` keeps
//! deny anchors. `object` owns the private handle representation, with traversal,
//! operations and host adapters beneath it so no raw handle escapes.

mod error;
mod exclusion;
pub(crate) mod json;
mod names;
mod object;
mod trace;

#[cfg(test)]
pub(crate) use error::directory_refusal_for_test;
pub(crate) use error::{dangling, failure_trace, io_failure, is_loop, unsynced};
pub use exclusion::{Excluded, Hold, Walked};
pub use names::{EntryName, NotAnEntryName, RootName};
#[cfg(any(target_os = "linux", windows))]
pub(crate) use object::Installing;
#[cfg(any(test, feature = "test-util"))]
pub(crate) use object::steps_taken;
pub(crate) use object::{
    Access, Listed, StagedWrite, Streamed, Streaming, open_discard, sync_dir, sync_dir_chain,
    walk_dir_made, walk_dir_made_synced,
};
pub use object::{
    Authority, Dir, DirEntry, Handle, LeafMode, Listing, Located, Op, Reading, ScopedDir, Unjudged,
    Visibility, Vouched, Writing, exists, kind_of, walk, walk_dir, walk_lenient,
};
#[cfg(unix)]
pub(crate) use object::{Vetted, walk_with};
pub use trace::{Kind, ObjectId, RealPath, Stat, Step, Trace};
