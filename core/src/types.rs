//! Runtime types for the ral evaluator.
//!
//! A re-export façade: every type lives in a private submodule and is named
//! through this path, so the rest of the tree never tracks which one owns what.

mod env;
pub use env::{Binding, Env, EnvVars};

pub(crate) mod signature;
pub(crate) use signature::{PreludeMap, Signature, lookup};

pub use shell::repl::{PluginEntry, ReplScratch};

mod value;
pub(crate) use value::Leaf;
pub use value::Value;
#[cfg(test)]
pub(crate) use value::{block_over, captured, deep_block_chain};

mod closure;
pub use closure::Closure;

// What the exec boundary refuses, declared once for the two sides that read it:
// the checker before the spawn, `runtime::command::vet` at it.

mod handler;
pub(crate) use handler::{
    FrameHandle, FrameKind, HandlerArity, HandlerEntry, HandlerFrame, HandlerLookup, HandlerRole,
    refused_arm, validate_handler_arity,
};

// The shared state behind `Value::Handle`.
mod handle;
#[cfg(test)]
pub(crate) use handle::idle_handle;
pub(crate) use handle::pins_running_work;
pub(crate) use handle::surface::{DeferredSurface, FlushGuard};
pub use handle::{CompletedHandle, HandleInner, HandleState, Latch, SurfaceBuffer};

// A boundary's checked type, and what a door admits against it.
mod admit;
pub use admit::Mismatch;
pub(crate) use admit::pointer_token;

mod builtin;
pub use crate::typecheck::builtins::Convention;
pub use builtin::{BuiltinBody, BuiltinEntry, BuiltinTable};
pub(crate) use builtin::{Set, language_constants};

// The inner of `Value::List`.
mod list;
pub use list::List;

// The inner of `Value::Map`.
mod map;
pub use map::Map;

// The inner of `Value::Bytes`.
// Here because `Value::Bytes` holds one.
pub use crate::first_order::Bytes;

mod error;
pub use error::{Error, Status, outcome_value};

mod exit_hints;
pub use exit_hints::ExitHints;

mod flow;
pub(crate) use flow::name_failure;
pub use flow::{Break, Escape, Settled};

// `sig` rides along with the coercions: both sit below the builtins and the
// capability layer, which reach them without importing each other.
mod coerce;
pub(crate) use coerce::{decode_utf8_strict, sig_hint};
pub use coerce::{settings_map, sig};

pub use shell::modules::Modules;

pub use shell::cwd::Cwd;

pub mod audit;
pub(crate) use audit::AuditStart;
pub use audit::{Audit, AuditFragment, AuditIo, CapturePolicy, TrailScope, epoch_us, report_value};

// Here because the session speaks them: the records `fact` declares.
pub use crate::fact::{
    Check, CommandOrigin, Decision, ErrorRecord, LeaseClass, Observation, Observed, Reason,
    Resource, WorkerId, WriteOutcome,
};

// Here because every observation carries one.
pub use crate::source::CallSite;

mod mooring;
pub use mooring::{
    DeferredSink, Desk, EnquiryDesk, EventSink, Fork, Mooring, NO_DESK, NO_DESK_HINT,
    NO_DESK_STATUS, Nursery, NurseryId, SurfaceSink,
};
pub(crate) use mooring::{NurseryGuard, TerminalAccess};

mod shell;
pub(crate) use shell::Context;
pub use shell::hooks::{DefaultPolicy, Hook, HookName, HookSig, Namespace, RegisterError};
pub use shell::{DEFAULT_STACK_LIMIT, LocalState, SessionState, Shell};

pub(crate) use shell::workers::{CapReached, LeaseChain};
pub use shell::workers::{Done, DoneEvent, ReapCause, ReapNotice, WorkerEntry, WorkerLease};

pub use shell::bindings::{BindingLease, Pruned};
pub use shell::notice::Notice;
