//! Runtime types for the ral evaluator.
//!
//! A re-export façade: every type lives in a private submodule and is named
//! through this path, so the rest of the tree never tracks which one owns what.

mod env;
pub use env::{Binding, Env, EnvVars};

pub(crate) mod signature;
pub(crate) use signature::{PreludeMap, Signature, lookup};

pub use shell::repl::{PluginEntry, ReplScratch};

mod capability;
pub(crate) use capability::meet_insert;
pub use capability::{
    Capabilities, EditorPolicy, ExecGrant, ExecProjection, ExecRule, FsPolicy, FsProjection,
    FsRules, GrantStack, Meet, SandboxProjection, ShellPolicy, Verdict, Widen,
};

mod value;
pub use value::{Value, fmt_float, fmt_lambda, fmt_native};
#[cfg(test)]
pub(crate) use value::{block_over, captured, deep_block_chain};

mod closure;
pub use closure::Closure;

// What the exec boundary refuses, declared once for the two sides that read it:
// the checker before the spawn, `runtime::command::vet` at it.
mod exec_arg;
pub(crate) use exec_arg::RefusedArg;

mod handler;
pub(crate) use handler::{
    FrameHandle, HandlerArity, HandlerEntry, HandlerFrame, HandlerLookup, HandlerRole,
    HandlerStack, refused_arm, validate_handler_arity,
};

// The shared state behind `Value::Handle`.
mod handle;
#[cfg(test)]
pub(crate) use handle::idle_handle;
pub(crate) use handle::pins_running_work;
pub use handle::{CompletedHandle, HandleInner, HandleState, SurfaceBuffer};

// A boundary's checked type, and what a door admits against it.
mod site;
pub(crate) use site::pointer_token;
pub use site::{Fixings, Mismatch, Site};

mod builtin;
pub(crate) use builtin::LANGUAGE_CONSTANTS;
pub use builtin::{BuiltinBody, BuiltinEntry, BuiltinTable, Convention};

// The inner of `Value::List`.
mod list;
pub use list::List;

// The inner of `Value::Map`.
mod map;
pub use map::Map;

// The inner of `Value::String`.
mod string;
pub use string::Str;

// The inner of `Value::Bytes`.
mod bytes;
pub use bytes::Bytes;

mod error;
pub use error::{Error, Status};

// The projection of an `Error` into the record `try` and the report envelope
// read; here because its two sides — the error and the record — are.
pub use crate::evaluator::scope::error_record_of;

mod flow;
pub use flow::{Break, Escape, PolicyError, Settled};

// `sig` rides along with the coercions: both sit below the builtins and the
// capability layer, which reach them without importing each other.
mod coerce;
pub use coerce::{as_list, as_map, settings_map, sig};
pub(crate) use coerce::{as_map_ref, sig_hint};

pub use shell::modules::Modules;

pub use shell::cwd::Cwd;

mod audit;
pub use audit::{Audit, AuditFragment, AuditIo, CapturePolicy, TrailScope, epoch_us, report_value};

mod observation;
pub(crate) use observation::site_value;
pub use observation::{CommandOrigin, Decision, Observation, Observed, WriteOutcome};

// Here because every observation carries one.
pub use crate::diagnostic::CallSite;

mod mooring;
pub use mooring::{
    DeferredSink, Desk, EnquiryDesk, EventSink, Fork, Mooring, NO_DESK, NO_DESK_HINT,
    NO_DESK_STATUS, Nursery, NurseryId, SurfaceSink,
};
pub(crate) use mooring::{NurseryGuard, TerminalAccess};

mod shell;
pub use shell::hooks::{
    DefaultPolicy, Hook, HookName, HookSig, Namespace, RegisterError, TerminalPolicy,
};
pub use shell::{Context, DEFAULT_STACK_LIMIT, LocalState, SessionState, Shell};

pub(crate) use shell::workers::{CapReached, WorkerRegistry};
pub use shell::workers::{LeaseClass, ReapCause, ReapNotice, WorkerEntry, WorkerId, WorkerLease};

pub use shell::bindings::{BindingLease, BindingPruneNotice, LargeBindingNotice};

// The signature every session-lived, capability-reachable thing answers
// through its own representation, so the folds over them are written once.
mod resident;
pub use resident::Resident;
