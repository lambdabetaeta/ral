//! Runtime types for the ral evaluator.
//!
//! This module is a re-export façade.  All types are defined in submodules;
//! consumers use `crate::types::*` or name types through this path so that
//! the rest of the tree does not need to track which submodule owns what.

// Lexical environment and process env-var overrides.  See types/env.rs.
mod env;
pub use env::{Binding, Env, EnvVars, EnvVarsIter};

// Evaluator control-flow counters.  See types/shell/control.rs.
pub use shell::control::ControlState;

// REPL-only scratch state (editor / chpwd hook).  See types/shell/repl.rs.
pub use shell::repl::ReplScratch;

// Capability layer: ExecPolicy, FsPolicy, EditorPolicy, ShellPolicy,
// SandboxProjection/BindSpec/CheckSpec, Capabilities + meet.  See
// types/capability.rs.
mod capability;
pub use capability::{
    Capabilities, EditorPolicy, ExecDir, ExecMap, ExecPolicy, ExecProjection, FsPolicy,
    FsProjection, GrantStack, Join, Meet, SandboxBindSpec, SandboxProjection, ShellPolicy,
};

// Runtime values: Value, Handle*, HandlerFrame, fmt_lambda.  See
// types/value.rs.
mod value;
pub(crate) use value::pins_running_work;
pub use value::{
    BuiltinBody, BuiltinEntry, BuiltinTable, CompletedHandle, FrameHandle, HandleInner,
    HandleState, HandlerArity, HandlerEntry, HandlerFrame, HandlerRole, HandlerStack,
    SurfaceBuffer, Value, fmt_lambda, validate_handler_arity,
};

// List (Value::List inner), opaque newtype around imbl::Vector<Value>.
// See types/list.rs.
mod list;
pub use list::List;

// Map (Value::Map inner), opaque newtype around imbl::OrdMap<String,
// Value>.  See types/map.rs.
mod map;
pub use map::Map;

// Runtime errors and the body-result split helper.  See types/error.rs.
mod error;
pub use error::{BodyResult, Error, Status, split};

// Phase-2 control-flow types: Escape, Break, Tail, TailCall, Control,
// Settled, Raw. See types/flow.rs. Tail/TailCall/Control/Raw are
// pub(crate) by design.
mod flow;
pub use flow::{Break, Escape, Settled};
pub(crate) use flow::{Control, Raw, Tail, TailCall};

// Error constructors and Value→Map coercions.  See types/coerce.rs.
mod coerce;
pub use coerce::{as_map, sig};
pub(crate) use coerce::{as_map_ref, sig_hint};

// Module-loader state.  See types/shell/modules.rs.
pub use shell::modules::Modules;

// Logical shell cwd (current + OLDPWD companion).  See types/shell/cwd.rs.
pub use shell::cwd::Cwd;

// Audit collector, execution tree, source positions.  See types/audit.rs.
mod audit;
pub use audit::{
    Audit, AuditFragment, AuditIo, AuditTime, AuditTrail, CallSite, CapturePolicy, ExecNode,
    ExecNodeKind, LocationCursor, STDERR_CAP_BYTES, epoch_us,
};

// Shell state, Context.  See types/shell.rs.
mod shell;
pub use shell::hooks::{
    DefaultPolicy, Hook, HookName, HookSig, Namespace, RegisterError, TerminalPolicy,
};
pub use shell::{
    Context, DEFAULT_RECURSION_LIMIT, DeferredSink, Desk, EnquiryDesk, EventSink, LocalState,
    Mobile, MobileSnapshot, SessionState, Shell, SurfaceSink, TerminalLoan, TurnState,
};
pub(crate) use shell::{TerminalAccess, ThunkBody};

// Per-shell worker registry: id, lease class, entry, the frame lease, and
// the reap-notice record.  See types/shell/workers.rs.
pub(crate) use shell::workers::{CapReached, WorkerRegistry};
pub use shell::workers::{LeaseClass, ReapCause, ReapNotice, WorkerEntry, WorkerId, WorkerLease};

// Per-shell binding-lease ledger: the idle-call policy and its prune
// notices.  See types/shell/bindings.rs.
pub use shell::bindings::{BindingLease, BindingPruneNotice, LargeBindingNotice};

// The resident signature every session-lived, capability-reachable chapter
// answers through its own representation.  See types/resident.rs.
mod resident;
pub use resident::Resident;
