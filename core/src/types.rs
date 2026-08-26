//! Runtime types for the ral evaluator.
//!
//! A re-export façade: every type lives in a private submodule and is named
//! through this path, so the rest of the tree never tracks which one owns what.

mod env;
pub use env::{Binding, Env, EnvVars, EnvVarsIter};
pub(crate) use env::{BindingMap, NativeMap, PreludeMap};

pub use shell::repl::ReplScratch;

mod capability;
pub use capability::{
    Capabilities, EditorPolicy, ExecMap, ExecPolicy, ExecProjection, FsPolicy, FsProjection,
    FsRules, GrantStack, Join, Meet, SandboxProjection, ShellPolicy,
};

mod value;
#[cfg(test)]
pub(crate) use value::deep_block_chain;
pub use value::{Value, fmt_float, fmt_lambda, fmt_native};

mod closure;
pub use closure::Closure;

// What the exec boundary refuses, declared once for the two sides that read it:
// the checker before the spawn, `runtime::command::vet` at it.
mod exec_arg;
pub(crate) use exec_arg::RefusedArg;

mod handler;
pub use handler::{
    FrameHandle, HandlerArity, HandlerEntry, HandlerFrame, HandlerLookup, HandlerRole,
    HandlerStack, validate_handler_arity,
};

// The shared state behind `Value::Handle`.
mod handle;
pub(crate) use handle::pins_running_work;
pub use handle::{CompletedHandle, HandleInner, HandleState, SurfaceBuffer};

mod builtin;
pub use builtin::{BuiltinBody, BuiltinEntry, BuiltinTable, Convention};

// The inner of `Value::List`.
mod list;
pub use list::List;

// The inner of `Value::Map`.
mod map;
pub use map::Map;

mod error;
pub use error::{Error, Status};

mod flow;
pub use flow::{Break, Escape, PolicyError, Settled};

// `sig` rides along with the coercions: both sit below the builtins and the
// capability layer, which reach them without importing each other.
mod coerce;
pub use coerce::{as_list, as_map, sig};
pub(crate) use coerce::{as_map_ref, sig_hint};

pub use shell::modules::Modules;

pub use shell::cwd::Cwd;

mod audit;
pub use audit::{
    Audit, AuditFragment, AuditIo, CapturePolicy, STDERR_CAP_BYTES, TrailScope, epoch_us,
    tree_value,
};

mod observation;
pub use observation::{CommandOrigin, Decision, Observation, Observed, WriteOutcome};

// Here because every observation carries one.
pub use crate::diagnostic::CallSite;

mod mooring;
pub use mooring::{
    DeferredSink, Desk, EnquiryDesk, EventSink, Fork, Mooring, NO_DESK, NO_DESK_STATUS, Nursery,
    NurseryId, SurfaceSink,
};
pub(crate) use mooring::{NurseryGuard, TerminalAccess};

mod shell;
pub use shell::hooks::{
    DefaultPolicy, Hook, HookName, HookSig, Namespace, RegisterError, TerminalPolicy,
};
pub use shell::{Context, DEFAULT_STACK_LIMIT, LocalState, SessionState, Shell};

pub(crate) use shell::workers::{CapReached, WorkerRegistry};
pub use shell::workers::{LeaseClass, ReapCause, ReapNotice, WorkerEntry, WorkerId, WorkerLease};

pub use shell::detached::{DetachPolicy, Reservation};

pub use shell::bindings::{BindingLease, BindingPruneNotice, LargeBindingNotice};

// The signature every session-lived, capability-reachable thing answers
// through its own representation, so the folds over them are written once.
mod resident;
pub use resident::Resident;
