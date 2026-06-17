//! Runtime capability decisions over the dynamic capability stack.
//!
//! **This module owns every yes/no answer the runtime asks of the
//! capability stack** — exec, fs, editor, shell, and the OS-renderable
//! `SandboxProjection`.  Each decision is a `check_*(&Context, …)`
//! function that folds the whole dynamic stack (`ctx.grants`) with a
//! `meet`, so a verdict reflects authority intersected across every
//! layer, never a single frame.  The capability *types*
//! (`Capabilities`, `ExecPolicy`, `FsPolicy`, …) live in
//! `crate::types::capability`.
//!
//! ## Sub-modules
//!
//! - `enforce`   — the point-of-use gates: exec, fs (both audited),
//!   editor, shell, and head admission.
//! - `sandbox`   — the OS-renderable `SandboxProjection` builder.
//! - `exec`      — per-layer and stack-level exec verdict evaluation.
//! - `decode`    — walk a `grant` capability `Value` map into a frozen
//!   `Capabilities`, resolving every sigil against a `FreezeCtx`.
//! - `load`      — load a `.ral` capability profile into a `Capabilities`.
//!
//! The `Meet` and `Join` traits live alongside the types they operate
//! on, in [`crate::types::capability`].

mod decode;
mod enforce;
mod exec;
mod load;
mod sandbox;

pub(crate) use decode::decode_capability_map;
pub(crate) use enforce::{
    FsOp, admits_head, check_editor_read, check_editor_tui, check_editor_write, check_exec_args,
    check_fs_op, check_shell_chdir,
};
pub use load::{apply_session_profiles, load_capabilities_from_path, load_capabilities_from_str};
pub(crate) use sandbox::sandbox_projection;
