//! Runtime types for the ral evaluator.
//!
//! A re-export façade: every type lives in a private submodule and is named
//! through this path, so the rest of the tree never tracks which one owns what.

mod env;
pub use env::{Binding, Env, EnvVars, EnvVarsIter};

pub use shell::control::ControlState;

pub use shell::repl::ReplScratch;

mod capability;
pub use capability::{
    Capabilities, EditorPolicy, ExecMap, ExecPolicy, ExecProjection, FsPolicy, FsProjection,
    FsRules, GrantStack, Join, Meet, SandboxProjection, ShellPolicy,
};

mod value;
pub use value::{Value, fmt_lambda, fmt_native};

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
pub use builtin::{BuiltinBody, BuiltinEntry, BuiltinTable};

// The inner of `Value::List`.
mod list;
pub use list::List;

// The inner of `Value::Map`.
mod map;
pub use map::Map;

mod error;
pub use error::{BodyResult, Error, Status, split};

mod flow;
pub use flow::{Break, Escape, PolicyError, Settled};
pub(crate) use flow::{Control, Raw, Tail, TailCall};

// `sig` rides along with the coercions: both sit below the builtins and the
// capability layer, which reach them without importing each other.
mod coerce;
pub use coerce::{as_list, as_map, sig};
pub(crate) use coerce::{as_map_ref, sig_hint};

pub use shell::modules::Modules;

pub use shell::cwd::Cwd;

mod audit;
pub(crate) use audit::NodeOutcome;
pub use audit::{
    Audit, AuditFragment, AuditIo, AuditTime, AuditTrail, CapturePolicy, ExecNode, ExecNodeKind,
    STDERR_CAP_BYTES, epoch_us,
};

// Here because audit nodes and capability checks carry one.
pub use crate::diagnostic::CallSite;

mod mooring;
pub use mooring::{
    DeferredSink, Desk, EnquiryDesk, EventSink, Mooring, Nursery, NurseryId, SurfaceSink,
};
pub(crate) use mooring::{NurseryGuard, TerminalAccess};

mod shell;
pub(crate) use shell::ThunkBody;
pub use shell::hooks::{
    DefaultPolicy, Hook, HookName, HookSig, Namespace, RegisterError, TerminalPolicy,
};
pub use shell::{Context, DEFAULT_RECURSION_LIMIT, LocalState, Mobile, SessionState, Shell};

pub(crate) use shell::workers::{CapReached, WorkerRegistry};
pub use shell::workers::{LeaseClass, ReapCause, ReapNotice, WorkerEntry, WorkerId, WorkerLease};

pub use shell::detached::{DetachPolicy, Reservation};

pub use shell::bindings::{BindingLease, BindingPruneNotice, LargeBindingNotice};

// The signature every session-lived, capability-reachable thing answers
// through its own representation, so the folds over them are written once.
mod resident;
pub use resident::Resident;
