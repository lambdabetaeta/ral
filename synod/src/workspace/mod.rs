//! The honest account of a run: the folder's shape before, its shape
//! after, and the difference named.
//!
//! The agent works directly in the granted folder, and nothing here ever
//! puts anything back.  What this module promises is narrower and keepable:
//! the folder is stat-walked once when the conversation opens and again
//! when a turn settles, and the two records are compared into a report the
//! user can read.  No file is ever opened, so a folder costs one stat-walk
//! to record no matter how many gigabytes are in it — and the report says
//! plainly where that leaves it short, marking a file whose timestamp moved
//! but whose size did not as *touched* rather than claiming an edit it
//! cannot prove.
//!
//! The GUI is the product surface; this module's public API is its seam.
//! Everything public here is serde-serializable for that reason, and none
//! of it is exposed on the command line.

pub mod changes;
pub mod manifest;
pub mod report;

pub use changes::Change;
pub use manifest::{EntryKind, Manifest, covers};
pub use report::{JobReport, job_report};
