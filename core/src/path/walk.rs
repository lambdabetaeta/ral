//! One resolver: a spelling walked to its object, and the transcript a judgment reads.
//!
//! A name is never authority. [`walk`] descends through directory handles,
//! splicing links itself; its [`Trace`] records what it opened. Operations
//! use those same handles, only after the grant stack or sandbox vouched for
//! them through [`Authority`].
//!
//! `names` validates spellings; `trace` holds the evidence; `exclusion` keeps
//! deny anchors. `held` owns the private handle representation, with traversal,
//! operations and host adapters beneath it so no raw handle escapes.

mod error;
mod exclusion;
mod held;
mod names;
mod trace;

#[cfg(test)]
pub(crate) use error::directory_refusal_for_test;
pub(crate) use error::{dangling, directory_refused, is_loop, loop_trace};
pub use exclusion::{Excluded, Hold, Walked};
#[cfg(any(test, feature = "test-util"))]
pub(crate) use held::steps_taken;
pub(crate) use held::{
    Access, Listed, StagedWrite, Streamed, Streaming, open_discard, walk_dir_made,
};
pub use held::{
    Authority, Dir, DirEntry, Installing, Judged, LeafMode, Listing, Lists, Located, Op, Peeks,
    Reading, Reads, ScopedDir, Unjudged, Visibility, Writing, exists, is_dir, kind_of, walk,
    walk_dir, walk_lenient,
};
#[cfg(unix)]
pub(crate) use held::{Vetted, walk_with};
pub use names::{EntryName, NotAnEntryName, RootName};
pub use trace::{Kind, ObjectId, RealPath, Stat, Step, Trace};
