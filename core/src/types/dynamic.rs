//! Dynamic context (σ).
//!
//! The components of shell state that flow with dynamic extent: shell
//! environment vars, current working directory, capability restriction
//! stack, effect-handler stack, and invocation positional args.  These
//! travel together as a unit through every same-thread thunk and every
//! thread spawn — the `inherit_from`/`spawn_thread` paths clone
//! `Dynamic` whole, and `return_to` drops it.
//!
//! Replaces the old `Ambient` struct plus the lifted `script_args`
//! flat field on `Shell`.  After this refactor, `Ambient` no longer
//! exists; everything dynamic-extent lives here.
//!
//! `script_args` is grouped here because it inherits with the caller
//! (positional arguments propagate from script to sourced module to
//! function call without rebinding).  Unlike `env_vars` / `cwd` /
//! `capabilities_stack` / `handler_stack`, `within` and `grant` do not
//! modify it — it's "dynamic" in the inherit-with-caller sense, not
//! the attenuable-by-`within` sense.
//!
//! Wire format: `Dynamic` is *not* `Serialize` / `Deserialize`.  The
//! sandbox IPC layer (`sandbox::ipc`) defines an `IpcAmbient` mirror
//! holding the four ambient sub-fields; `script_args` is packed as a
//! separate wire field.  Wire layout is preserved across this
//! refactor.

use crate::types::{Capabilities, HandlerFrame};
use std::collections::HashMap;
use std::path::PathBuf;

/// Dynamically-scoped runtime context.
#[derive(Debug, Clone, Default)]
pub struct Dynamic {
    /// Process environment overrides (`within [shell: ...]`).
    pub env_vars: HashMap<String, String>,
    /// Working directory override (`within [dir: ...]`).
    pub cwd: Option<PathBuf>,
    /// Capability restriction stack — innermost last.
    pub capabilities_stack: Vec<Capabilities>,
    /// `within [handlers: …, handler: …]` effect-handler stack —
    /// innermost last.
    pub handler_stack: Vec<HandlerFrame>,
    /// Invocation positional args (`$args`, `$1`, ...) passed on the
    /// command line or by `source`.  Inherited with caller; not
    /// modified by `within` / `grant`.
    pub script_args: Vec<String>,
}
