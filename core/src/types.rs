//! Runtime types for the ral evaluator.
//!
//! This module is a re-export façade.  All types are defined in submodules;
//! consumers use `crate::types::*` or name types through this path so that
//! the rest of the tree does not need to track which submodule owns what.

// Lexical environment (ρ).  Closure-captured.  See types/env.rs.
mod env;
pub use env::Env;

// Dynamic context (σ).  Clone-into-child, drop-on-return.  See
// types/dynamic.rs.
mod dynamic;
pub use dynamic::Dynamic;

// Evaluator control-flow counters.  See types/control.rs.
mod control;
pub use control::ControlState;

// REPL-only scratch state (editor / chpwd hook).  See types/repl.rs.
mod repl;
pub use repl::ReplScratch;

// Capability layer: ExecPolicy, FsPolicy, EditorPolicy, ShellPolicy,
// SandboxProjection/BindSpec/CheckSpec, Capabilities + meet.  See
// types/capability.rs.
mod capability;
pub use capability::{
    Capabilities, EditorPolicy, ExecPolicy, FsPolicy, RawCapabilities, SandboxBindSpec,
    SandboxCheckSpec, SandboxProjection, ShellPolicy,
};

// Runtime values: Value, Handle*, HandlerFrame, fmt_block.  See
// types/value.rs.
mod value;
pub use value::{fmt_block, HandlerFrame, HandleInner, HandleState, Value};

// Runtime errors and eval signals.  See types/error.rs.
mod error;
pub use error::{Error, ErrorKind, EvalSignal};

// Plugin context and editor state.  See types/plugin.rs.
mod plugin;
pub use plugin::{EditorState, HighlightSpan, PluginContext, PluginInputs, PluginOutputs};

// Alias and plugin registry.  See types/registry.rs.
mod registry;
pub use registry::{AliasEntry, AliasOrigin, LoadedPlugin, Modules, Registry};

// Audit collector, execution tree, source positions.  See types/audit.rs.
mod audit;
pub use audit::{epoch_us, Audit, CallSite, ExecNode, ExecNodeKind, Location};

// Shell state, HeritableSnapshot, CommandHead.  See types/shell.rs.
mod shell;
pub use shell::{CommandHead, HeritableSnapshot, Shell, DEFAULT_RECURSION_LIMIT};
pub(crate) use shell::unique_strings;
