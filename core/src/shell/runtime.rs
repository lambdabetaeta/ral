//! Runtime types for the ral evaluator.
//!
//! A re-export façade: every type lives in a private submodule and is named
//! through this path, so the rest of the tree never tracks which one owns what.

mod env;
pub use env::{Binding, Env};

pub(crate) mod signature;
pub(crate) use signature::{PreludeMap, Signature, lookup};

mod value;
pub use value::{ValClos, View};
#[cfg(test)]
pub(crate) use value::{block_over, captured, deep_block_chain};

// What the exec boundary refuses, declared once for the two sides that read it:
// the checker before the spawn, `shell::exec::command::check_argv` at it.

mod handler;
pub use handler::{FrameHandle, HandlerEntry};
pub(crate) use handler::{FrameKind, HandlerArity, HandlerFrame, HandlerLookup};

// A boundary's checked type, and what a door admits against it.
mod admit;
pub use admit::Mismatch;
pub(crate) use admit::pointer_token;

pub use crate::typecheck::builtins::Convention;

mod list;
pub use list::{List, ListRef};

mod map;
pub use map::{Map, MapRef};

// Shared byte strings.
pub use crate::data::Bytes;

mod error;
pub use crate::error::{Error, FrameName, Prov, Unwound};
pub use error::outcome_value;

mod exit_hints;
pub use exit_hints::ExitHints;

mod flow;
pub use flow::{Break, End, Escape, Settled, Stop};
pub(crate) use flow::{UNREPORTED, panic_text};

mod walk;
pub use walk::AdmittedWalk;

// `sig` rides along with the coercions: both sit below the builtins and the
// capability layer, which reach them without importing each other.
mod coerce;
pub(crate) use coerce::{decode_utf8_strict, ill_typed, invalid};

pub mod audit;
pub(crate) use audit::DisplacedSite;
pub use audit::{
    Audit, AuditFragment, CapturePolicy, TrailScope, epoch_us, report_envelope, report_value,
};

// Here because the session speaks them: the records `fact` declares.
pub use crate::fact::{
    Check, CommandOrigin, Decision, Done, ErrorFact, Observation, Observed, Reason, Resource,
    WorkerId, WriteOutcome,
};

// Here because every observation carries one.
pub use crate::source::CallSite;

mod mooring;
pub(crate) use mooring::NurseryGuard;
pub use mooring::{
    DeferredSink, Desk, EnquiryDesk, EventSink, Fork, Mooring, NO_DESK, NO_DESK_HINT, Nursery,
    NurseryId, SurfaceSink, TitlePolicy, no_desk,
};

mod state;
pub(crate) use state::Context;
pub use state::hooks::{DefaultPolicy, Hook, HookName, HookSig, Namespace, RegisterError};
pub(crate) use state::json::RawContext;
pub use state::{Bequest, DEFAULT_STACK_LIMIT, LocalState, Runtime, SessionState, Shell};
pub(crate) use state::{Image, Terms};

pub use state::workers::{DoneEvent, Owner, ReapCause, ReapNotice, WatchEvent, WorkerEntry};

pub use state::bindings::{BindingLease, Pruned};
pub use state::notice::Notice;
