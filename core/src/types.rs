//! Runtime types for the ral evaluator.
//!
//! This module is a re-export façade.  All types are defined in submodules;
//! consumers use `crate::types::*` or name types through this path so that
//! the rest of the tree does not need to track which submodule owns what.

// Lexical environment and process env-var overrides.
mod env;
pub use env::{Binding, Env, EnvVars, EnvVarsIter};

// Evaluator control-flow counters.
pub use shell::control::ControlState;

// REPL-only scratch state (editor / chpwd hook).
pub use shell::repl::ReplScratch;

// Capability layer: ExecPolicy, FsPolicy, EditorPolicy, ShellPolicy,
// SandboxProjection/BindSpec/CheckSpec, Capabilities + meet.
mod capability;
pub use capability::{
    Capabilities, EditorPolicy, ExecDir, ExecMap, ExecPolicy, ExecProjection, FsPolicy,
    FsProjection, GrantStack, Join, Meet, SandboxBindSpec, SandboxProjection, ShellPolicy,
};

// Runtime values: Value and lambda rendering.
mod value;
pub use value::{Value, fmt_lambda};

// The user handler stack: frames, entries, and two-pass dispatch.
mod handler;
pub use handler::{
    FrameHandle, HandlerArity, HandlerEntry, HandlerFrame, HandlerRole, HandlerStack,
    validate_handler_arity,
};

// Concurrency substrate backing Value::Handle.
mod handle;
pub(crate) use handle::pins_running_work;
pub use handle::{CompletedHandle, HandleInner, HandleState, SurfaceBuffer};

// Builtin command-binding table: BuiltinBody, BuiltinEntry, BuiltinTable.
mod builtin;
pub use builtin::{BuiltinBody, BuiltinEntry, BuiltinTable};

// List (Value::List inner), opaque newtype around imbl::Vector<Value>.
mod list;
pub use list::List;

// Map (Value::Map inner), opaque newtype around imbl::OrdMap<String, Value>.
mod map;
pub use map::Map;

// Runtime errors and the body-result split helper.
mod error;
pub use error::{BodyResult, Error, Status, split};

// Control-flow types: Escape, Break, Tail, TailCall, Control, Settled,
// Raw. Tail/TailCall/Control/Raw are pub(crate) by design.
mod flow;
pub use flow::{Break, Escape, Settled};
pub(crate) use flow::{Control, Raw, Tail, TailCall};

// Error constructors and Value→Map coercions.
mod coerce;
pub use coerce::{as_list, as_map, sig};
pub(crate) use coerce::{as_map_ref, sig_hint};

// Module-loader state.
pub use shell::modules::Modules;

// Logical shell cwd (current + OLDPWD companion).
pub use shell::cwd::Cwd;

// Audit collector and execution tree.
mod audit;
pub use audit::{
    Audit, AuditFragment, AuditIo, AuditTime, AuditTrail, CapturePolicy, ExecNode, ExecNodeKind,
    STDERR_CAP_BYTES, epoch_us,
};

// Turn-local source cursor and its call-site snapshot.  See diagnostic.rs.
pub use crate::diagnostic::{CallSite, LocationCursor};

// Shell state, Context.
mod shell;
pub use shell::hooks::{
    DefaultPolicy, Hook, HookName, HookSig, Namespace, RegisterError, TerminalPolicy,
};
pub use shell::{
    Context, DEFAULT_RECURSION_LIMIT, DeferredSink, Desk, EnquiryDesk, EventSink, LocalState,
    Mobile, MobileSnapshot, Nursery, NurseryId, SessionState, Shell, SurfaceSink, TerminalLoan,
    TurnState,
};
pub(crate) use shell::{TerminalAccess, ThunkBody};

// Per-shell worker registry: id, lease class, entry, the frame lease, and
// the reap-notice record.
pub(crate) use shell::workers::{CapReached, WorkerRegistry};
pub use shell::workers::{LeaseClass, ReapCause, ReapNotice, WorkerEntry, WorkerId, WorkerLease};

// Per-shell binding-lease ledger: the idle-call policy and its prune
// notices.
pub use shell::bindings::{BindingLease, BindingPruneNotice, LargeBindingNotice};

// The resident signature every session-lived, capability-reachable chapter
// answers through its own representation.
mod resident;
pub use resident::Resident;
