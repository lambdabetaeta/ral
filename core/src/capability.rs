//! Runtime capability verdicts over the dynamic grant stack.
//!
//! Every decision here folds the whole stack (`ctx.grants`) with a
//! `meet`, so a verdict is authority intersected across all layers and
//! never a single frame.  The capability types and their lattice
//! algebra live in `crate::types::capability` — as does the one verdict
//! not taken here, [`crate::types::GrantStack::permits_detach`], which
//! gates a verb rather than an access.

mod decode;
mod deputy;
mod enforce;
mod exec;
mod fs;
mod load;
mod sandbox;
mod table;

pub use decode::decode_capability_map;
pub use deputy::deputy_prefixes;
pub(crate) use enforce::{
    Admitted, admits_head, check_device_name, check_editor_read, check_editor_tui,
    check_editor_write, check_exec, check_fs_exact, check_fs_op, check_shell_chdir, exec_respelled,
};
pub(crate) use exec::Program;
pub use fs::FsOp;
pub use load::{apply_session_profiles, load_capabilities_from_path, load_capabilities_from_str};
pub(crate) use sandbox::sandbox_projection;
