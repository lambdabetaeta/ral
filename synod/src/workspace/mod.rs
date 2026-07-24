//! The safety net around a run: checkpoint before, report and undo after.
//!
//! The agent works directly in the granted folder.  What makes that
//! tolerable: the folder is recorded before the job, the difference is
//! reported after, and anything can be put back — per file or whole job —
//! with edits made *after* the job surfacing as conflicts, never silently
//! overwritten.  Bytes about to be replaced or removed are always kept in
//! the store first, so no undo destroys anything.  Undoing an undo is not
//! a one-click operation yet: the bytes survive in the store, the button
//! does not.
//!
//! The GUI is the product surface; this module's public API is its seam.
//! Everything public here is serde-serializable for that reason, and none
//! of it is exposed on the command line.

pub mod changes;
pub mod history;
pub mod manifest;
pub mod report;
pub mod restore;

pub use changes::Change;
pub use history::{HistoryStore, Moment};
pub use manifest::{EntryKind, LARGE_FOLDER_BYTES};
pub use report::{job_report, undo_all, undo_file};
pub use restore::{Resolution, RestoreOutcome};
